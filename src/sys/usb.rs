// usb.rs
//! USB, as far as it goes so far: start the host controller and ask the device on its root
//! port (on the Pi 3 B+, the LAN7515's first hub) to describe itself.

use rustypi_core::usb::{Class, DescriptorType, DeviceDescriptor, HubDescriptor, SetupPacket, Speed};
use crate::drivers::usb::{Controller, Host, UsbError};

/// What `probe` found.
pub struct Probe {
    pub controller: Controller,
    pub speed: Speed,
    pub device: DeviceDescriptor,
    /// The device's hub descriptor, if it is a hub.
    pub hub: Option<HubDescriptor>,
}

#[derive(Debug)]
pub enum ProbeError {
    Usb(UsbError),
    Descriptor(rustypi_core::usb::DescriptorError),
}

impl From<UsbError> for ProbeError {
    fn from(error: UsbError) -> Self {
        ProbeError::Usb(error)
    }
}

impl From<rustypi_core::usb::DescriptorError> for ProbeError {
    fn from(error: rustypi_core::usb::DescriptorError) -> Self {
        ProbeError::Descriptor(error)
    }
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            ProbeError::Usb(error) => write!(f, "{error}"),
            ProbeError::Descriptor(error) => write!(f, "{error}"),
        }
    }
}

/// Starts (or restarts) the host controller and reads the root device's descriptors, at
/// address 0, where a device answers until it is given one. Takes a second or so.
pub fn probe() -> Result<Probe, ProbeError> {
    let mut host = Host::start()?;
    // Packets of 8 always work for the first 8 bytes, which say how big they may be.
    let first = host.control(0, 8, SetupPacket::get_descriptor(DescriptorType::DEVICE, 0, 8))?;
    let max_packet = DeviceDescriptor::max_packet_size(first)? as u16;
    let length = DeviceDescriptor::LENGTH as u16;
    let device = DeviceDescriptor::parse(host.control(0, max_packet, SetupPacket::get_descriptor(DescriptorType::DEVICE, 0, length))?)?;
    let hub = if device.class == Class::HUB {
        Some(HubDescriptor::parse(host.control(0, max_packet, SetupPacket::get_hub_descriptor(9))?)?)
    } else {
        None
    };
    Ok(Probe { controller: host.controller, speed: host.port.speed, device, hub })
}
