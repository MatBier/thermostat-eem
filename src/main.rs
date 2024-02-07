//! # Thermostat_EEM
//!
//! Firmware for "Thermostat EEM", a multichannel temperature controller.

#![no_std]
#![no_main]

use core::fmt::Write;

pub mod hardware;
pub mod net;
pub mod output_channel;
pub mod statistics;

use panic_probe as _; // global panic handler

use enum_iterator::all;
use hardware::{
    ad7172::AdcChannel,
    adc::AdcPhy,
    adc::{sm::StateMachine, Adc, AdcCode},
    adc_internal::AdcInternal,
    dac::{Dac, DacCode},
    gpio::{Gpio, PoePower},
    hal,
    pwm::{Limit, Pwm},
    system_timer::SystemTimer,
    OutputChannelIdx,
};
use miniconf::Tree;
use net::{Alarm, NetworkState, NetworkUsers};
use serde::Serialize;
use statistics::{Buffer, Statistics};
use systick_monotonic::{ExtU64, Systick};

#[derive(Clone, Debug, Tree)]
pub struct Settings {
    /// Specifies the telemetry output period in seconds.
    ///
    /// # Path
    /// `telemetry_period`
    ///
    /// # Value
    /// Any positive non-zero value. Will be rounded to milliseconds.
    telemetry_period: f32,

    /// Array of settings for the Thermostat output channels.
    ///
    /// # Path
    /// `output_channel/<n>`
    /// * `<n> := [0, 1, 2, 3]` specifies which channel to configure.
    ///
    /// # Value
    /// See [output_channel::OutputChannel]
    #[tree(depth(2))]
    output_channel: [output_channel::OutputChannel; 4],

    /// Alarm settings.
    ///
    /// # Path
    /// `Alarm`
    ///
    /// # Value
    /// See [Alarm]
    #[tree(depth(3))]
    alarm: Alarm,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            telemetry_period: 1.0,
            output_channel: Default::default(),
            alarm: Default::default(),
        }
    }
}

/// Telemetry for various quantities that are continuously monitored by eg. the MCU ADC.
#[derive(Serialize, Copy, Clone, Default, Debug)]
pub struct Monitor {
    p3v3_voltage: f32,
    p5v_voltage: f32,
    p12v_voltage: f32,
    p12v_current: f32,
    /// Measurement of the output reference voltages.
    output_vref: [f32; 4],
    /// Measurement of the output currents.
    output_current: [f32; 4],
    /// Measurement of the output voltages.
    output_voltage: [f32; 4],
    /// See [PoePower]
    poe: PoePower,
    /// Overtemperature status.
    overtemp: bool,
}

/// Thermostat-EEM Telemetry.
#[derive(Serialize, Copy, Clone, Debug, Default)]
pub struct Telemetry {
    /// see [Monitor]
    monitor: Monitor,
    /// `[<adc>][<channel>]` array of [Statistics]. `None` for disabled channels.
    statistics: [[Option<Statistics>; 4]; 4],
    /// Alarm status for each enabled input channel. `None` for disabled channels.
    alarm: [[Option<bool>; 4]; 4],
    /// Output current in Amperes for each Thermostat output channel.
    output_current: [f32; 4],
}

#[rtic::app(device = hal::stm32, peripherals = true, dispatchers=[DCMI, JPEG, SDMMC])]
mod app {
    use super::*;

    #[monotonic(binds = SysTick, default = true)]
    type Mono = Systick<1_000>; // 1ms resolution

    #[shared]
    struct Shared {
        network: NetworkUsers<Settings, Telemetry, 4>,
        settings: Settings,
        telemetry: Telemetry,
        gpio: Gpio,
        temperature: [[f64; 4]; 4], // input temperature array in °C. Organized as [Adc_idx,  Channel_idx].
        statistics_buff: [[Buffer; 4]; 4], // input statistics buffer for processing telemetry. Organized as [Adc_idx,  Channel_idx].
    }

    #[local]
    struct Local {
        adc_sm: StateMachine<Adc>,
        dac: Dac,
        pwm: Pwm,
        adc_internal: AdcInternal,
        iir_state: [[f64; 4]; 4],
    }

    #[init]
    fn init(c: init::Context) -> (Shared, Local, init::Monotonics) {
        // Initialize monotonic
        let clock = SystemTimer::new(|| monotonics::now().ticks());

        // setup Thermostat hardware
        let thermostat = hardware::setup::setup(c.device, clock);

        let settings = Settings::default();

        let mut id = heapless::String::<32>::new();
        write!(&mut id, "{}", thermostat.net.mac_address).unwrap();

        let network = NetworkUsers::new(
            thermostat.net.stack,
            thermostat.net.phy,
            clock,
            &id,
            option_env!("BROKER").unwrap_or("mqtt"),
            settings.clone(),
            thermostat.metadata,
        );

        let local = Local {
            adc_sm: thermostat.adc_sm,
            pwm: thermostat.pwm,
            adc_internal: thermostat.adc_internal,
            iir_state: Default::default(),
            dac: thermostat.dac,
        };

        let shared = Shared {
            network,
            settings: settings.clone(),
            telemetry: Default::default(),
            gpio: thermostat.gpio,
            temperature: Default::default(),
            statistics_buff: Default::default(),
        };

        // Apply initial settings
        settings_update::spawn(settings).unwrap();
        ethernet_link::spawn().unwrap();
        telemetry_task::spawn().unwrap();
        mqtt_alarm::spawn().unwrap();

        (
            shared,
            local,
            init::Monotonics(Systick::new(
                c.core.SYST,
                thermostat.clocks.sysclk().to_Hz(),
            )),
        )
    }

    #[idle(shared=[network])]
    fn idle(mut c: idle::Context) -> ! {
        loop {
            c.shared.network.lock(|net| match net.update() {
                NetworkState::SettingsChanged(_path) => {
                    settings_update::spawn(net.miniconf.settings().clone()).unwrap()
                }
                NetworkState::Updated => {}
                NetworkState::NoChange => {}
            })
        }
    }

    #[task(priority = 1, local=[pwm], shared=[settings, gpio], capacity=1)]
    fn settings_update(mut c: settings_update::Context, mut settings: Settings) {
        // Limit y_min and y_max values here. Will be incorporated into miniconf response later.
        for ch in settings.output_channel.iter_mut() {
            ch.iir.set_max(
                ch.iir
                    .max()
                    .clamp(-DacCode::MAX_CURRENT as _, DacCode::MAX_CURRENT as _),
            );
            ch.iir.set_min(
                ch.iir
                    .min()
                    .clamp(-DacCode::MAX_CURRENT as _, DacCode::MAX_CURRENT as _),
            );
        }

        let pwm = c.local.pwm;
        for ch in all::<OutputChannelIdx>() {
            let mut s = settings.output_channel[ch as usize];
            let current_limits = s.finalize_settings(); // clamp limits and normalize weights
            pwm.set_limit(Limit::Voltage(ch), s.voltage_limit).unwrap();
            // give 5% extra headroom for PWM current limits
            pwm.set_limit(Limit::PositiveCurrent(ch), current_limits[0])
                .unwrap();
            pwm.set_limit(Limit::NegativeCurrent(ch), current_limits[1])
                .unwrap();
            c.shared.gpio.lock(|gpio| {
                gpio.set_shutdown(ch, s.shutdown.into());
                gpio.set_led(ch.into(), (!s.shutdown).into()) // fix leds to channel state
            });
        }

        // Verify settings and make them available
        c.shared.settings.lock(|current_settings| {
            *current_settings = settings;
        });
    }

    #[task(priority = 1, local=[adc_internal], shared=[network, settings, telemetry, gpio, statistics_buff])]
    fn telemetry_task(mut c: telemetry_task::Context) {
        let mut telemetry: Telemetry = c.shared.telemetry.lock(|telemetry| *telemetry);
        let adc_int = c.local.adc_internal;
        telemetry.monitor.p3v3_voltage = adc_int.read_p3v3_voltage();
        telemetry.monitor.p5v_voltage = adc_int.read_p5v_voltage();
        telemetry.monitor.p12v_voltage = adc_int.read_p12v_voltage();
        telemetry.monitor.p12v_current = adc_int.read_p12v_current();
        for ch in all::<OutputChannelIdx>() {
            let idx = ch as usize;
            telemetry.monitor.output_vref[idx] = adc_int.read_output_vref(ch);
            telemetry.monitor.output_voltage[idx] = adc_int.read_output_voltage(ch);
            telemetry.monitor.output_current[idx] = adc_int.read_output_current(ch);
        }
        c.shared.gpio.lock(|gpio| {
            telemetry.monitor.overtemp = gpio.overtemp();
            telemetry.monitor.poe = gpio.poe();
        });

        // Finalize temperature telemetry and reset buffer
        for phy_i in 0..4 {
            for cfg_i in 0..4 {
                c.shared.statistics_buff.lock(|buff| {
                    telemetry.statistics[phy_i][cfg_i] = buff[phy_i][cfg_i].into();
                    buff[phy_i][cfg_i] = Default::default();
                });
            }
        }

        c.shared
            .network
            .lock(|network| network.telemetry.publish(&telemetry));

        // TODO: validate telemetry period.
        let telemetry_period = c.shared.settings.lock(|settings| settings.telemetry_period);
        telemetry_task::spawn_after(((telemetry_period * 1000.0) as u64).millis()).unwrap();
    }

    #[task(priority = 1, shared=[network, settings, temperature, telemetry])]
    fn mqtt_alarm(mut c: mqtt_alarm::Context) {
        let alarm = c.shared.settings.lock(|settings| settings.alarm.clone());
        if alarm.armed {
            let temperatures = c.shared.temperature.lock(|temp| *temp);
            let mut alarms = [[None; 4]; 4];
            let mut alarm_state = false;
            for phy_i in 0..4 {
                for cfg_i in 0..4 {
                    if let Some(l) = &alarm.temperature_limits[phy_i][cfg_i] {
                        let a = !(l[0]..l[1]).contains(&(temperatures[phy_i][cfg_i] as _));
                        alarms[phy_i][cfg_i] = Some(a);
                        alarm_state |= a;
                    }
                }
            }
            c.shared
                .telemetry
                .lock(|telemetry| telemetry.alarm = alarms);
            c.shared
                .network
                .lock(|net| net.telemetry.publish_alarm(&alarm.target, &alarm_state));
        }
        // Note that you have to wait for a full period of the previous setting first for a change of period to take affect.
        mqtt_alarm::spawn_after(((alarm.period * 1000.0) as u64).millis()).unwrap();
    }

    #[task(priority = 2, local=[dac], capacity=4)]
    fn convert_current_and_set_dac(
        c: convert_current_and_set_dac::Context,
        output_ch: OutputChannelIdx,
        current: f32,
    ) {
        let dac_code = DacCode::try_from(current).unwrap();
        c.local.dac.set(output_ch, dac_code);
    }

    #[task(priority = 2, shared=[temperature, settings, telemetry], local=[iir_state], capacity = 4)]
    fn process_output_channel(mut c: process_output_channel::Context, output_ch: OutputChannelIdx) {
        let idx = output_ch as usize;
        let current = (c.shared.settings, c.shared.temperature).lock(|settings, temperature| {
            settings.output_channel[idx].update(temperature, &mut c.local.iir_state[idx])
        });
        c.shared
            .telemetry
            .lock(|tele| tele.output_current[idx] = current);
        convert_current_and_set_dac::spawn(output_ch, current).unwrap();
    }

    // Higher priority than telemetry but lower than adc data readout.
    #[task(priority = 2, shared=[temperature, statistics_buff], capacity=4)]
    fn convert_adc_code(
        mut c: convert_adc_code::Context,
        phy: AdcPhy,
        ch: AdcChannel,
        adc_code: AdcCode,
    ) {
        let (phy_i, ch_i) = (phy as usize, ch as usize);
        let temperature = adc_code.into();
        c.shared.temperature.lock(|temp| {
            temp[phy_i][ch_i] = temperature;
        });
        c.shared.statistics_buff.lock(|stat_buff| {
            stat_buff[phy_i][ch_i].update(temperature);
        });
        // Start processing when the last ADC has been read out.
        // This implies a zero-order hold (aka the input sample will not be updated at every signal processing step) if more than one channel is enabled on an ADC.
        if phy == AdcPhy::Three {
            for ch in all::<OutputChannelIdx>() {
                process_output_channel::spawn(ch).unwrap();
            }
        }
    }

    #[task(priority = 3, binds = EXTI15_10, local=[adc_sm])]
    fn adc_readout(c: adc_readout::Context) {
        let (phy, ch, adc_code) = c.local.adc_sm.handle_interrupt();
        convert_adc_code::spawn(phy, ch, adc_code).unwrap();
    }

    #[task(priority = 1, shared=[network])]
    fn ethernet_link(mut c: ethernet_link::Context) {
        c.shared
            .network
            .lock(|network| network.processor.handle_link());
        ethernet_link::spawn_after(1.secs()).unwrap();
    }

    #[task(binds = ETH, priority = 1)]
    fn eth(_: eth::Context) {
        unsafe { hal::ethernet::interrupt_handler() }
    }
}
