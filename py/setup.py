#!/usr/bin/python3
"""
Setup script for Thermostat EEM
"""
from setuptools import setup, find_packages

setup(
    name="thermostat",
    packages=find_packages(),
    # Keep versions in Cargo.toml and py/setup.py synchronized.
    version="0.1.0",
    description="Thermostat Utilities",
    author="QUARTIQ GmbH",
    license="MIT",
    install_requires=[
        "stabilizer@git+https://github.com/quartiq/stabilizer@becc1c72#subdirectory=py",
        # Note: Keep in-sync with `Cargo.toml`
        "miniconf-mqtt@git+https://github.com/quartiq/miniconf@miniconf-v0.10.1#subdirectory=py/miniconf-mqtt",
    ],
)
