use crate::hardware::system_timer;
use smoltcp_nal::smoltcp;
use stm32h7xx_hal::hal::digital::v2::OutputPin;

use crate::hardware::SRC_MAC;

use super::hal::{
    self as hal,
    ethernet::{self, PHY},
    gpio::GpioExt,
    prelude::*,
};

use super::{
    adc_internal::{AdcInternal, OutIPins, OutUPins, SupplyPins},
    EthernetPhy, LEDs, NetworkStack,
};

use defmt::info;

const NUM_TCP_SOCKETS: usize = 4;
const NUM_UDP_SOCKETS: usize = 1;
const NUM_SOCKETS: usize = NUM_UDP_SOCKETS + NUM_TCP_SOCKETS;

pub struct NetStorage {
    pub ip_addrs: [smoltcp::wire::IpCidr; 1],

    // Note: There is an additional socket set item required for the DHCP socket.
    pub sockets: [smoltcp::iface::SocketStorage<'static>; NUM_SOCKETS + 1],
    pub tcp_socket_storage: [TcpSocketStorage; NUM_TCP_SOCKETS],
    pub udp_socket_storage: [UdpSocketStorage; NUM_UDP_SOCKETS],
    pub neighbor_cache: [Option<(smoltcp::wire::IpAddress, smoltcp::iface::Neighbor)>; 8],
    pub routes_cache: [Option<(smoltcp::wire::IpCidr, smoltcp::iface::Route)>; 8],
}

pub struct UdpSocketStorage {
    rx_storage: [u8; 1024],
    tx_storage: [u8; 2048],
    tx_metadata: [smoltcp::storage::PacketMetadata<smoltcp::wire::IpEndpoint>; 10],
    rx_metadata: [smoltcp::storage::PacketMetadata<smoltcp::wire::IpEndpoint>; 10],
}

impl UdpSocketStorage {
    const fn new() -> Self {
        Self {
            rx_storage: [0; 1024],
            tx_storage: [0; 2048],
            tx_metadata: [smoltcp::storage::PacketMetadata::<smoltcp::wire::IpEndpoint>::EMPTY; 10],
            rx_metadata: [smoltcp::storage::PacketMetadata::<smoltcp::wire::IpEndpoint>::EMPTY; 10],
        }
    }
}

#[derive(Copy, Clone)]
pub struct TcpSocketStorage {
    rx_storage: [u8; 1024],
    tx_storage: [u8; 1024],
}

impl TcpSocketStorage {
    const fn new() -> Self {
        Self {
            rx_storage: [0; 1024],
            tx_storage: [0; 1024],
        }
    }
}

impl Default for NetStorage {
    fn default() -> Self {
        NetStorage {
            // Placeholder for the real IP address, which is initialized at runtime.
            ip_addrs: [smoltcp::wire::IpCidr::Ipv6(
                smoltcp::wire::Ipv6Cidr::SOLICITED_NODE_PREFIX,
            )],
            neighbor_cache: [None; 8],
            routes_cache: [None; 8],
            sockets: [smoltcp::iface::SocketStorage::EMPTY; NUM_SOCKETS + 1],
            tcp_socket_storage: [TcpSocketStorage::new(); NUM_TCP_SOCKETS],
            udp_socket_storage: [UdpSocketStorage::new(); NUM_UDP_SOCKETS],
        }
    }
}

/// The available networking devices on Thermostat.
pub struct NetworkDevices {
    pub stack: NetworkStack,
    pub phy: EthernetPhy,
    pub mac_address: smoltcp::wire::EthernetAddress,
}

/// The available hardware interfaces on Thermostat.
pub struct ThermostatDevices {
    pub net: NetworkDevices,
    pub leds: LEDs,
}

#[link_section = ".sram3.eth"]
/// Static storage for the ethernet DMA descriptor ring.
static mut DES_RING: ethernet::DesRing<{ super::TX_DESRING_CNT }, { super::RX_DESRING_CNT }> =
    ethernet::DesRing::new();

pub fn setup(
    device: stm32h7xx_hal::stm32::Peripherals,
    clock: system_timer::SystemTimer,
) -> ThermostatDevices {
    let pwr = device.PWR.constrain();
    let vos = pwr.freeze();

    // Enable SRAM3 for the ethernet descriptor ring.
    device.RCC.ahb2enr.modify(|_, w| w.sram3en().set_bit());

    // Clear reset flags.
    device.RCC.rsr.write(|w| w.rmvf().set_bit());

    let rcc = device.RCC.constrain();
    let ccdr = rcc
        .use_hse(8.mhz())
        .sysclk(400.mhz())
        .hclk(200.mhz())
        .per_ck(100.mhz())
        .pll2_p_ck(100.mhz())
        .pll2_q_ck(100.mhz())
        .freeze(vos, &device.SYSCFG);

    info!("--- Starting hardware setup");

    let mut delay = asm_delay::AsmDelay::new(asm_delay::bitrate::Hertz(ccdr.clocks.c_ck().0));

    // Take GPIOs
    let gpioa = device.GPIOA.split(ccdr.peripheral.GPIOA);
    let gpiob = device.GPIOB.split(ccdr.peripheral.GPIOB);
    let gpioc = device.GPIOC.split(ccdr.peripheral.GPIOC);
    // let gpiod = device.GPIOD.split(ccdr.peripheral.GPIOD);
    let gpioe = device.GPIOE.split(ccdr.peripheral.GPIOE);
    let gpiof = device.GPIOF.split(ccdr.peripheral.GPIOF);
    let gpiog = device.GPIOG.split(ccdr.peripheral.GPIOG);

    // Setup LEDs
    let mut leds = LEDs {
        led0: gpiog.pg9.into_push_pull_output(),
        led1: gpiog.pg10.into_push_pull_output(),
        led2: gpioe.pe8.into_push_pull_output(),
        led3: gpioe.pe10.into_push_pull_output(),
        led4: gpioe.pe12.into_push_pull_output(),
        led5: gpiog.pg15.into_push_pull_output(),
        led6: gpioe.pe15.into_push_pull_output(),
        led7: gpiog.pg8.into_push_pull_output(),
    };

    leds.led0.set_low().unwrap();
    leds.led1.set_low().unwrap();
    leds.led2.set_low().unwrap();
    leds.led3.set_low().unwrap();
    leds.led4.set_low().unwrap();
    leds.led5.set_low().unwrap();
    leds.led6.set_low().unwrap();
    leds.led7.set_low().unwrap();

    info!("-- Setup Ethernet");
    let mac_addr = smoltcp::wire::EthernetAddress(SRC_MAC);
    log::info!("EUI48: {}", mac_addr);

    // Setup network
    let net = {
        let ethernet_pins = {
            // Reset the PHY before configuring pins.
            let mut eth_phy_nrst = gpiog.pg14.into_push_pull_output();
            eth_phy_nrst.set_low().unwrap();
            delay.delay_us(200u8);
            eth_phy_nrst.set_high().unwrap();

            let rmii_ref_clk = gpioa
                .pa1
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_mdio = gpioa
                .pa2
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_mdc = gpioc
                .pc1
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_crs_dv = gpioa
                .pa7
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_rxd0 = gpioc
                .pc4
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_rxd1 = gpioc
                .pc5
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_tx_en = gpiog
                .pg11
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_txd0 = gpiog
                .pg13
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);
            let rmii_txd1 = gpiob
                .pb13
                .into_alternate_af11()
                .set_speed(hal::gpio::Speed::VeryHigh);

            (
                rmii_ref_clk,
                rmii_mdio,
                rmii_mdc,
                rmii_crs_dv,
                rmii_rxd0,
                rmii_rxd1,
                rmii_tx_en,
                rmii_txd0,
                rmii_txd1,
            )
        };

        info!("Ethernet PHY pins bound");
        // Configure the ethernet controller
        let (eth_dma, eth_mac) = ethernet::new(
            device.ETHERNET_MAC,
            device.ETHERNET_MTL,
            device.ETHERNET_DMA,
            ethernet_pins,
            // Note(unsafe): We only call this function once to take ownership of the
            // descriptor ring.
            unsafe { &mut DES_RING },
            mac_addr,
            ccdr.peripheral.ETH1MAC,
            &ccdr.clocks,
        );

        // Reset and initialize the ethernet phy.
        let mut lan8742a = ethernet::phy::LAN8742A::new(eth_mac.set_phy_addr(0));
        lan8742a.phy_reset();
        lan8742a.phy_init();

        unsafe { ethernet::enable_interrupt() };

        info!("Configure TCP/UDP buffers and neighbour/routing caches");
        // Note(unwrap): The hardware configuration function is only allowed to be called once.
        // Unwrapping is intended to panic if called again to prevent re-use of global memory.
        let store = cortex_m::singleton!(: NetStorage = NetStorage::default()).unwrap();

        store.ip_addrs[0] = smoltcp::wire::IpCidr::new(
            smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED),
            0,
        );

        let mut routes = smoltcp::iface::Routes::new(&mut store.routes_cache[..]);
        routes
            .add_default_ipv4_route(smoltcp::wire::Ipv4Address::UNSPECIFIED)
            .unwrap();

        let neighbor_cache = smoltcp::iface::NeighborCache::new(&mut store.neighbor_cache[..]);

        let mut interface = smoltcp::iface::InterfaceBuilder::new(eth_dma, &mut store.sockets[..])
            .hardware_addr(smoltcp::wire::HardwareAddress::Ethernet(mac_addr))
            .neighbor_cache(neighbor_cache)
            .ip_addrs(&mut store.ip_addrs[..])
            .routes(routes)
            .finalize();

        interface.add_socket(smoltcp::socket::Dhcpv4Socket::new());

        for storage in store.tcp_socket_storage[..].iter_mut() {
            let tcp_socket = {
                let rx_buffer = smoltcp::socket::TcpSocketBuffer::new(&mut storage.rx_storage[..]);
                let tx_buffer = smoltcp::socket::TcpSocketBuffer::new(&mut storage.tx_storage[..]);

                smoltcp::socket::TcpSocket::new(rx_buffer, tx_buffer)
            };

            interface.add_socket(tcp_socket);
        }

        for storage in store.udp_socket_storage[..].iter_mut() {
            let udp_socket = {
                let rx_buffer = smoltcp::socket::UdpSocketBuffer::new(
                    &mut storage.rx_metadata[..],
                    &mut storage.rx_storage[..],
                );
                let tx_buffer = smoltcp::socket::UdpSocketBuffer::new(
                    &mut storage.tx_metadata[..],
                    &mut storage.tx_storage[..],
                );

                smoltcp::socket::UdpSocket::new(rx_buffer, tx_buffer)
            };

            interface.add_socket(udp_socket);
        }

        let random_seed = {
            let mut rng = device.RNG.constrain(ccdr.peripheral.RNG, &ccdr.clocks);
            let mut data = [0u8; 4];
            rng.fill(&mut data).unwrap();
            data
        };

        let mut stack = smoltcp_nal::NetworkStack::new(interface, clock);

        stack.seed_random_port(&random_seed);

        info!("-- Setup Ethernet done.");

        NetworkDevices {
            stack,
            phy: lan8742a,
            mac_address: mac_addr,
        }
    };

    info!("setup internal ADCs");

    let out_u_pins: OutUPins = (
        gpioc.pc3.into_analog(),
        gpioa.pa0.into_analog(),
        gpioa.pa3.into_analog(),
        gpioa.pa4.into_analog(),
    );
    let out_i_pins: OutIPins = (
        gpioa.pa5.into_analog(),
        gpioa.pa6.into_analog(),
        gpiob.pb0.into_analog(),
        gpiob.pb1.into_analog(),
    );
    let supply_pins: SupplyPins = (
        gpioc.pc0.into_analog(),
        gpioc.pc2.into_analog(),
        gpiof.pf7.into_analog(),
        gpiof.pf8.into_analog(),
    );

    let mut adc_int = AdcInternal::new(
        &mut delay,
        &ccdr.clocks,
        (ccdr.peripheral.ADC12, ccdr.peripheral.ADC3),
        (device.ADC1, device.ADC2, device.ADC3),
        supply_pins,
        out_u_pins,
        out_i_pins,
    );

    info!("P12v: {:?} V", adc_int.read_p12v());
    info!("P5v: {:?} V", adc_int.read_p5v());
    info!("P3v3: {:?} V", adc_int.read_p3v3());
    info!("I12v: {:?} A", adc_int.read_i12v());

    info!("--- Hardware setup done.");

    ThermostatDevices { net, leds }
}
