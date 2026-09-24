// usb/tree.rs
//! Enumeration: giving each device on the bus an address and a configuration, and walking
//! down through hubs to find everything behind them. Generic over the host controller
//! (`Bus`), so it can be tested against a pretend bus.
//!
//! A device answers at address 0 from when its port is reset until it is given an address,
//! so only one may be there at a time: ports are reset one after another, and a port whose
//! device isn't enumerated (it failed, or needs split transactions) is disabled again.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use super::{Class, Configuration, DescriptorError, DescriptorType, DeviceDescriptor, HubDescriptor, PortFeature, PortStatus, SetupPacket, Speed};

/// Endpoint 0 of a device, as a control transfer needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub address: u8,
    pub max_packet: u16,
    pub speed: Speed,
}

/// A host controller, as far as enumeration needs one.
pub trait Bus {
    type Error;

    /// A control transfer to endpoint 0 of `target`. The data stage, if `setup` has one, sends
    /// `data` (OUT) or fills it (IN), `setup.length` bytes. Returns the bytes received.
    fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, Self::Error>;

    fn delay_ms(&mut self, ms: u32);
}

/// Hubs chained deeper than the USB 2.0 limit (five, besides the root).
const MAX_DEPTH: usize = 6;
/// Addresses go from 1 to this.
const MAX_ADDRESS: u8 = 127;
/// USB 2.0 timings: after SET_ADDRESS, and after a port reset completes.
const SET_ADDRESS_RECOVERY_MS: u32 = 2;
const RESET_RECOVERY_MS: u32 = 10;
/// How long a hub gets to finish resetting a port, checked every `RESET_POLL_MS`.
const RESET_TIMEOUT_MS: u32 = 500;
const RESET_POLL_MS: u32 = 10;

#[derive(Debug)]
pub enum Error<E> {
    Bus(E),
    Descriptor(DescriptorError),
    OutOfAddresses,
    /// The hub didn't finish resetting the port.
    ResetTimedOut,
    /// The port wasn't enabled after its reset.
    NotEnabled,
    TooDeep,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Bus(error) => write!(f, "{error}"),
            Error::Descriptor(error) => write!(f, "{error}"),
            Error::OutOfAddresses => write!(f, "out of device addresses"),
            Error::ResetTimedOut => write!(f, "port reset timed out"),
            Error::NotEnabled => write!(f, "port not enabled after reset"),
            Error::TooDeep => write!(f, "hubs chained too deep"),
        }
    }
}

impl<E> From<DescriptorError> for Error<E> {
    fn from(error: DescriptorError) -> Self {
        Error::Descriptor(error)
    }
}

/// A device, addressed and configured.
#[derive(Debug)]
pub struct Device<E> {
    pub address: u8,
    pub speed: Speed,
    pub descriptor: DeviceDescriptor,
    pub configuration: Configuration,
    pub hub: Option<Hub<E>>,
}

impl<E> Device<E> {
    /// Endpoint 0, for control transfers to it.
    pub fn target(&self) -> Target {
        Target { address: self.address, max_packet: self.descriptor.max_packet_size as u16, speed: self.speed }
    }

    /// This device and everything behind it, depth first, each with how many hubs deep it is.
    pub fn walk(&self) -> Vec<(usize, &Device<E>)> {
        let mut found = Vec::new();
        self.walk_into(0, &mut found);
        found
    }

    fn walk_into<'a>(&'a self, depth: usize, found: &mut Vec<(usize, &'a Device<E>)>) {
        found.push((depth, self));
        for port in self.hub.iter().flat_map(|hub| &hub.ports) {
            if let Port::Device(device) = port {
                device.walk_into(depth + 1, found);
            }
        }
    }
}

#[derive(Debug)]
pub struct Hub<E> {
    pub descriptor: HubDescriptor,
    /// Port 1 first.
    pub ports: Vec<Port<E>>,
}

/// What is on a hub's port.
#[derive(Debug)]
pub enum Port<E> {
    Empty,
    Device(Device<E>),
    /// A low or full speed device behind a high speed hub, which only split transactions
    /// reach. Its port is disabled for now.
    NeedsSplit(Speed),
    /// Something is there, but enumerating it failed. Its port is disabled.
    Failed(Error<E>),
}

/// Enumerates the device on the root port, just reset, at speed `speed`, and everything
/// behind it if it is a hub.
pub fn enumerate<B: Bus>(bus: &mut B, speed: Speed) -> Result<Device<B::Error>, Error<B::Error>> {
    Enumerator { bus, next_address: 1, bus_speed: speed }.device(speed, 0)
}

struct Enumerator<'a, B: Bus> {
    bus: &'a mut B,
    next_address: u8,
    /// The root port's speed: at high speed, slower devices behind hubs need split
    /// transactions.
    bus_speed: Speed,
}

impl<B: Bus> Enumerator<'_, B> {
    fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, Error<B::Error>> {
        self.bus.control(target, setup, data).map_err(Error::Bus)
    }

    /// Enumerates the device answering at address 0, `depth` hubs below the root.
    fn device(&mut self, speed: Speed, depth: usize) -> Result<Device<B::Error>, Error<B::Error>> {
        // Packets of 8 always work for the first 8 bytes, which say how big they may be.
        let mut bytes = [0; DeviceDescriptor::LENGTH];
        let default = Target { address: 0, max_packet: 8, speed };
        let read = self.control(default, SetupPacket::get_descriptor(DescriptorType::DEVICE, 0, 8), &mut bytes[..8])?;
        let max_packet = DeviceDescriptor::max_packet_size(&bytes[..read])? as u16;

        if self.next_address > MAX_ADDRESS {
            return Err(Error::OutOfAddresses);
        }
        let address = self.next_address;
        self.next_address += 1;
        self.control(Target { max_packet, ..default }, SetupPacket::set_address(address), &mut [])?;
        self.bus.delay_ms(SET_ADDRESS_RECOVERY_MS);
        let target = Target { address, max_packet, speed };

        let read = self.control(target, SetupPacket::get_descriptor(DescriptorType::DEVICE, 0, DeviceDescriptor::LENGTH as u16), &mut bytes)?;
        let descriptor = DeviceDescriptor::parse(&bytes[..read])?;

        let mut head = [0; 9];
        let read = self.control(target, SetupPacket::get_descriptor(DescriptorType::CONFIGURATION, 0, 9), &mut head)?;
        let total = Configuration::total_length(&head[..read])?;
        let mut whole = vec![0; total as usize];
        let read = self.control(target, SetupPacket::get_descriptor(DescriptorType::CONFIGURATION, 0, total), &mut whole)?;
        let configuration = Configuration::parse(&whole[..read])?;
        self.control(target, SetupPacket::set_configuration(configuration.value), &mut [])?;

        let is_hub = descriptor.class == Class::HUB || configuration.interfaces.iter().any(|i| i.class == Class::HUB);
        let hub = if is_hub { Some(self.hub(target, depth)?) } else { None };
        Ok(Device { address, speed, descriptor, configuration, hub })
    }

    /// Powers a hub's ports and enumerates what is on each.
    fn hub(&mut self, hub: Target, depth: usize) -> Result<Hub<B::Error>, Error<B::Error>> {
        let mut bytes = [0; 9];
        let read = self.control(hub, SetupPacket::get_hub_descriptor(9), &mut bytes)?;
        let descriptor = HubDescriptor::parse(&bytes[..read])?;
        for port in 1..=descriptor.ports {
            self.control(hub, SetupPacket::set_port_feature(port, PortFeature::POWER), &mut [])?;
        }
        self.bus.delay_ms(descriptor.power_on_to_good_ms);
        let ports = (1..=descriptor.ports).map(|port| self.port(hub, port, depth)).collect();
        Ok(Hub { descriptor, ports })
    }

    fn port_status(&mut self, hub: Target, port: u8) -> Result<PortStatus, Error<B::Error>> {
        let mut bytes = [0; 4];
        self.control(hub, SetupPacket::get_port_status(port), &mut bytes)?;
        Ok(PortStatus::from_bytes(bytes))
    }

    /// Resets the port and enumerates the device on it, if there is one.
    fn port(&mut self, hub: Target, port: u8, depth: usize) -> Port<B::Error> {
        match self.port_status(hub, port) {
            Ok(status) if !status.connected() => return Port::Empty,
            Ok(_) => {}
            Err(error) => return Port::Failed(error),
        }
        let found = self.reset_port(hub, port).and_then(|speed| {
            if speed != Speed::High && self.bus_speed == Speed::High {
                return Ok(Port::NeedsSplit(speed));
            }
            if depth + 1 >= MAX_DEPTH {
                return Err(Error::TooDeep);
            }
            self.device(speed, depth + 1).map(Port::Device)
        });
        let found = found.unwrap_or_else(Port::Failed);
        if !matches!(found, Port::Device(_)) {
            // Out of the way of the next port's device at address 0. Nothing more to do if
            // the hub won't listen either.
            let _ = self.control(hub, SetupPacket::clear_port_feature(port, PortFeature::ENABLE), &mut []);
        }
        found
    }

    /// Resets a port with a device on it and returns the device's speed.
    fn reset_port(&mut self, hub: Target, port: u8) -> Result<Speed, Error<B::Error>> {
        self.control(hub, SetupPacket::set_port_feature(port, PortFeature::RESET), &mut [])?;
        let mut waited = 0;
        let status = loop {
            self.bus.delay_ms(RESET_POLL_MS);
            waited += RESET_POLL_MS;
            let status = self.port_status(hub, port)?;
            if status.reset_changed() || (!status.resetting() && status.enabled()) {
                break status;
            }
            if waited >= RESET_TIMEOUT_MS {
                return Err(Error::ResetTimedOut);
            }
        };
        self.control(hub, SetupPacket::clear_port_feature(port, PortFeature::RESET_CHANGE), &mut [])?;
        self.control(hub, SetupPacket::clear_port_feature(port, PortFeature::CONNECTION_CHANGE), &mut [])?;
        self.bus.delay_ms(RESET_RECOVERY_MS);
        if !status.enabled() {
            return Err(Error::NotEnabled);
        }
        Ok(status.speed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};

    /// A pretend device: its descriptors, and for a hub, its ports.
    struct FakeDevice {
        speed: Speed,
        device: [u8; 18],
        configuration: Vec<u8>,
        hub: Option<FakeHub>,
        address: Option<u8>,
        configured: Option<u8>,
        /// Answers nothing past its device descriptor.
        broken: bool,
    }

    struct FakeHub {
        ports: Vec<FakePort>,
    }

    #[derive(Default)]
    struct FakePort {
        device: Option<usize>,
        powered: bool,
        enabled: bool,
        reset_changed: bool,
    }

    /// A pretend bus: devices by index, the root first, reached by address.
    struct FakeBus {
        devices: Vec<FakeDevice>,
        /// The device answering at address 0: the root, then each one just reset.
        default: Option<usize>,
        slept_ms: u32,
    }

    fn device_descriptor(class: u8, vendor: u16, product: u16, max_packet: u8) -> [u8; 18] {
        let [v0, v1] = vendor.to_le_bytes();
        let [p0, p1] = product.to_le_bytes();
        [18, 1, 0, 2, class, 0, 0, max_packet, v0, v1, p0, p1, 0, 1, 0, 0, 0, 1]
    }

    /// A configuration with one interface of `class` and one interrupt IN endpoint.
    fn configuration(class: u8) -> Vec<u8> {
        vec![9, 2, 25, 0, 1, 1, 0, 0xE0, 0, 9, 4, 0, 0, 1, class, 0, 0, 0, 7, 5, 0x81, 3, 1, 0, 12]
    }

    fn fake(speed: Speed, class: u8, vendor: u16, product: u16, ports: usize) -> FakeDevice {
        let hub = (ports > 0).then(|| FakeHub { ports: (0..ports).map(|_| FakePort::default()).collect() });
        FakeDevice {
            speed,
            device: device_descriptor(class, vendor, product, if speed == Speed::High { 64 } else { 8 }),
            configuration: configuration(class),
            hub,
            address: None,
            configured: None,
            broken: false,
        }
    }

    impl FakeBus {
        /// The Pi 3 B+: a 4-port hub (0) at the root; on its port 1 a 3-port hub (1) with the
        /// LAN7800 (2) on port 1 and a low speed keyboard (3) on port 2; a flash drive (4) on
        /// port 3 of the first hub; its ports 2 and 4 empty.
        fn pi_3b_plus() -> FakeBus {
            let mut devices = vec![
                fake(Speed::High, 9, 0x0424, 0x2514, 4),
                fake(Speed::High, 9, 0x0424, 0x2514, 3),
                fake(Speed::High, 0xFF, 0x0424, 0x7800, 0),
                fake(Speed::Low, 0, 0x046D, 0xC31C, 0),
                fake(Speed::High, 0, 0x0781, 0x5567, 0),
            ];
            devices[0].hub.as_mut().unwrap().ports[0].device = Some(1);
            devices[0].hub.as_mut().unwrap().ports[2].device = Some(4);
            devices[1].hub.as_mut().unwrap().ports[0].device = Some(2);
            devices[1].hub.as_mut().unwrap().ports[1].device = Some(3);
            FakeBus { devices, default: Some(0), slept_ms: 0 }
        }

        fn find(&self, address: u8) -> Option<usize> {
            if address == 0 {
                self.default
            } else {
                self.devices.iter().position(|d| d.address == Some(address))
            }
        }

        fn port_status(&self, port: &FakePort) -> u32 {
            let connected = port.device.is_some();
            let mut status = connected as u32 | (port.enabled as u32) << 1 | (port.powered as u32) << 8;
            if let (Some(device), true) = (port.device, port.enabled) {
                match self.devices[device].speed {
                    Speed::Low => status |= 1 << 9,
                    Speed::High => status |= 1 << 10,
                    Speed::Full => {}
                }
            }
            status | (port.reset_changed as u32) << 20
        }
    }

    impl Bus for FakeBus {
        type Error = String;

        fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, String> {
            let index = self.find(target.address).ok_or("nobody at that address")?;
            let device = &self.devices[index];
            assert_eq!(target.speed, device.speed, "talking to a device at the wrong speed");
            let length = setup.length as usize;
            let reply = |bytes: &[u8], data: &mut [u8]| {
                let n = length.min(bytes.len());
                data[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            };
            let port = setup.index as usize;
            match (setup.request_type, setup.request) {
                (0x80, 6) if setup.value >> 8 == 1 => reply(&device.device.clone(), data),
                (_, _) if device.broken => Err("broken".to_string()),
                (0x80, 6) if setup.value >> 8 == 2 => reply(&device.configuration.clone(), data),
                (0x00, 5) => {
                    self.devices[index].address = Some(setup.value as u8);
                    self.default = None;
                    Ok(0)
                }
                (0x00, 9) => {
                    self.devices[index].configured = Some(setup.value as u8);
                    Ok(0)
                }
                (0xA0, 6) => {
                    let ports = device.hub.as_ref().ok_or("not a hub")?.ports.len() as u8;
                    reply(&[9, 0x29, ports, 0, 0, 50, 0, 0, 0xFF], data)
                }
                (0xA3, 0) => {
                    let status = self.port_status(&device.hub.as_ref().unwrap().ports[port - 1]);
                    reply(&status.to_le_bytes(), data)
                }
                (0x23, 3) => {
                    let hub_port = &mut self.devices[index].hub.as_mut().unwrap().ports[port - 1];
                    match setup.value {
                        8 => hub_port.powered = true,
                        4 if hub_port.powered && hub_port.device.is_some() => {
                            hub_port.enabled = true;
                            hub_port.reset_changed = true;
                            let child = hub_port.device.unwrap();
                            assert!(self.default.is_none(), "two devices at address 0");
                            self.default = Some(child);
                        }
                        feature => return Err(format!("set feature {feature}")),
                    }
                    Ok(0)
                }
                (0x23, 1) => {
                    let hub_port = &mut self.devices[index].hub.as_mut().unwrap().ports[port - 1];
                    match setup.value {
                        1 => {
                            hub_port.enabled = false;
                            if self.default == hub_port.device {
                                self.default = None;
                            }
                        }
                        20 => hub_port.reset_changed = false,
                        16 => {}
                        feature => return Err(format!("clear feature {feature}")),
                    }
                    Ok(0)
                }
                request => Err(format!("unexpected request {request:?}")),
            }
        }

        fn delay_ms(&mut self, ms: u32) {
            self.slept_ms += ms;
        }
    }

    #[test]
    fn the_whole_bus_is_found_through_both_hubs() {
        let mut bus = FakeBus::pi_3b_plus();
        let root = enumerate(&mut bus, Speed::High).unwrap();
        let found: Vec<_> = root.walk().iter().map(|(depth, d)| (*depth, d.address, d.descriptor.product)).collect();
        // Depth first: the second hub and what's on it come before the flash drive.
        assert_eq!(found, [(0, 1, 0x2514), (1, 2, 0x2514), (2, 3, 0x7800), (1, 4, 0x5567)]);
        // Everyone found was configured.
        for index in [0, 1, 2, 4] {
            assert_eq!(bus.devices[index].configured, Some(1));
        }
        let hub = root.hub.as_ref().unwrap();
        assert!(matches!(hub.ports[1], Port::Empty) && matches!(hub.ports[3], Port::Empty));
        assert!(bus.devices[0].hub.as_ref().unwrap().ports.iter().all(|port| port.powered));
    }

    #[test]
    fn slow_devices_behind_a_high_speed_hub_wait_for_split_transactions() {
        let mut bus = FakeBus::pi_3b_plus();
        let root = enumerate(&mut bus, Speed::High).unwrap();
        let Port::Device(second) = &root.hub.as_ref().unwrap().ports[0] else { panic!("second hub") };
        assert!(matches!(second.hub.as_ref().unwrap().ports[1], Port::NeedsSplit(Speed::Low)));
        // Its port was disabled again, so the flash drive after it could be reset.
        assert!(!bus.devices[1].hub.as_ref().unwrap().ports[1].enabled);
        assert_eq!(bus.devices[3].address, None);
    }

    #[test]
    fn a_failing_device_is_reported_and_the_rest_still_found() {
        let mut bus = FakeBus::pi_3b_plus();
        bus.devices[2].broken = true; // the LAN7800
        let root = enumerate(&mut bus, Speed::High).unwrap();
        let Port::Device(second) = &root.hub.as_ref().unwrap().ports[0] else { panic!("second hub") };
        assert!(matches!(&second.hub.as_ref().unwrap().ports[0], Port::Failed(Error::Bus(message)) if message == "broken"));
        assert!(!bus.devices[1].hub.as_ref().unwrap().ports[0].enabled);
        // The flash drive still got an address, the next free one after the broken device's.
        assert!(matches!(&root.hub.as_ref().unwrap().ports[2], Port::Device(drive) if drive.address == 4));
    }

    #[test]
    fn a_broken_root_device_is_an_error() {
        let mut bus = FakeBus::pi_3b_plus();
        bus.devices[0].broken = true;
        assert!(matches!(enumerate(&mut bus, Speed::High), Err(Error::Bus(_))));
    }

    #[test]
    fn hubs_wait_for_their_ports_to_power_up() {
        let mut bus = FakeBus::pi_3b_plus();
        enumerate(&mut bus, Speed::High).unwrap();
        // Two hubs' 100ms power-up at least, plus reset polling and recovery.
        assert!(bus.slept_ms >= 200, "slept {} ms", bus.slept_ms);
    }
}
