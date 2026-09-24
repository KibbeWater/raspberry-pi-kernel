// usb/tree.rs
//! Enumeration: giving each device on the bus an address and a configuration, and walking
//! down through hubs to find everything behind them. Generic over the host controller
//! (`Bus`), so it can be tested against a pretend bus.
//!
//! A device answers at address 0 from when its port is reset until it is given an address,
//! so only one may be there at a time: ports are reset one after another, and a port whose
//! device failed to enumerate is disabled again.
//!
//! On a high speed bus, low and full speed devices are reached through the transaction
//! translator of the nearest high speed hub above them, with split transactions: `Target`
//! says which hub and port.

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
    /// For a low or full speed device on a high speed bus: the translator its transactions
    /// are split through.
    pub translator: Option<Translator>,
}

/// A high speed hub's transaction translator, and the port of that hub the device is
/// behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translator {
    pub hub: u8,
    pub port: u8,
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
/// At least this long between powering a hub's ports and looking for devices on them: the
/// hub's own power-good time is too short for some devices to attach, like the Pi 3 B+'s
/// LAN7800 (Circle waits the same, for the same reason).
const MIN_POWER_UP_MS: u32 = 510;
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
    pub translator: Option<Translator>,
    pub descriptor: DeviceDescriptor,
    pub configuration: Configuration,
    pub hub: Option<Hub<E>>,
}

impl<E> Device<E> {
    /// Endpoint 0, for control transfers to it.
    pub fn target(&self) -> Target {
        Target {
            address: self.address,
            max_packet: self.descriptor.max_packet_size as u16,
            speed: self.speed,
            translator: self.translator,
        }
    }

    /// What kind of device it is: its own class, or if it leaves that to its interfaces, its
    /// first interface's.
    pub fn class(&self) -> Class {
        match self.descriptor.class {
            Class::PER_INTERFACE => self.configuration.interfaces.first().map_or(Class::PER_INTERFACE, |i| i.class),
            class => class,
        }
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
    /// Something is there, but enumerating it failed. Its port is disabled.
    Failed(Error<E>),
}

impl<E> Port<E> {
    pub fn device(&self) -> Option<&Device<E>> {
        match self {
            Port::Device(device) => Some(device),
            _ => None,
        }
    }
}

/// Device addresses in use.
struct Addresses {
    used: [bool; MAX_ADDRESS as usize + 1],
}

impl Addresses {
    const fn new() -> Self {
        Addresses { used: [false; MAX_ADDRESS as usize + 1] }
    }

    /// The lowest free address.
    fn take(&mut self) -> Option<u8> {
        let address = (1..=MAX_ADDRESS).find(|&address| !self.used[address as usize])?;
        self.used[address as usize] = true;
        Some(address)
    }

    /// Gives back `device`'s address and those of everything behind it.
    fn free<E>(&mut self, device: &Device<E>) {
        for (_, device) in device.walk() {
            self.used[device.address as usize] = false;
        }
    }
}

/// Everything on the bus, and the addresses it uses.
pub struct Tree<E> {
    /// The device on the root port.
    pub root: Device<E>,
    addresses: Addresses,
}

/// A device that came or went, from `Tree::poll_changes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Enumerated, with anything behind it.
    Added { address: u8 },
    /// Gone, with anything that was behind it.
    Removed { address: u8 },
}

/// Enumerates the device on the root port, just reset, at speed `speed`, and everything
/// behind it if it is a hub.
pub fn enumerate<B: Bus>(bus: &mut B, speed: Speed) -> Result<Tree<B::Error>, Error<B::Error>> {
    let mut addresses = Addresses::new();
    let root = Enumerator { bus, addresses: &mut addresses }.device(speed, None, 0)?;
    Ok(Tree { root, addresses })
}

impl<E> Tree<E> {
    /// Looks at every hub's ports for devices that came or went since the last look (or
    /// enumeration): forgets those that went, and enumerates those that came. Returns what
    /// changed.
    pub fn poll_changes<B: Bus<Error = E>>(&mut self, bus: &mut B) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut enumerator = Enumerator { bus, addresses: &mut self.addresses };
        enumerator.poll_hub(&mut self.root, 0, &mut changes);
        changes
    }

    /// The device at `address`, if there is one.
    pub fn device(&self, address: u8) -> Option<&Device<E>> {
        self.root.walk().into_iter().map(|(_, device)| device).find(|device| device.address == address)
    }
}

struct Enumerator<'a, B: Bus> {
    bus: &'a mut B,
    addresses: &'a mut Addresses,
}

impl<B: Bus> Enumerator<'_, B> {
    fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, Error<B::Error>> {
        self.bus.control(target, setup, data).map_err(Error::Bus)
    }

    /// Enumerates the device answering at address 0, `depth` hubs below the root.
    fn device(&mut self, speed: Speed, translator: Option<Translator>, depth: usize) -> Result<Device<B::Error>, Error<B::Error>> {
        // Packets of 8 always work for the first 8 bytes, which say how big they may be.
        let mut bytes = [0; DeviceDescriptor::LENGTH];
        let default = Target { address: 0, max_packet: 8, speed, translator };
        let read = self.control(default, SetupPacket::get_descriptor(DescriptorType::DEVICE, 0, 8), &mut bytes[..8])?;
        let max_packet = DeviceDescriptor::max_packet_size(&bytes[..read])? as u16;

        let address = self.addresses.take().ok_or(Error::OutOfAddresses)?;
        let device = self.addressed(Target { max_packet, ..default }, address, depth);
        if device.is_err() {
            // Its port is disabled, so nothing answers there any more.
            self.addresses.used[address as usize] = false;
        }
        device
    }

    /// The rest of enumerating a device, from giving it `address`.
    fn addressed(&mut self, default: Target, address: u8, depth: usize) -> Result<Device<B::Error>, Error<B::Error>> {
        let Target { max_packet, speed, translator, .. } = default;
        let mut bytes = [0; DeviceDescriptor::LENGTH];
        self.control(default, SetupPacket::set_address(address), &mut [])?;
        self.bus.delay_ms(SET_ADDRESS_RECOVERY_MS);
        let target = Target { address, max_packet, speed, translator };

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
        Ok(Device { address, speed, translator, descriptor, configuration, hub })
    }

    /// Powers a hub's ports and enumerates what is on each.
    fn hub(&mut self, hub: Target, depth: usize) -> Result<Hub<B::Error>, Error<B::Error>> {
        let mut bytes = [0; 9];
        let read = self.control(hub, SetupPacket::get_hub_descriptor(9), &mut bytes)?;
        let descriptor = HubDescriptor::parse(&bytes[..read])?;
        for port in 1..=descriptor.ports {
            self.control(hub, SetupPacket::set_port_feature(port, PortFeature::POWER), &mut [])?;
        }
        self.bus.delay_ms(descriptor.power_on_to_good_ms.max(MIN_POWER_UP_MS));
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
            if depth + 1 >= MAX_DEPTH {
                return Err(Error::TooDeep);
            }
            // Below a high speed hub, a slower device gets that hub's translator. Further
            // down, behind a full speed hub, it shares the translator that hub uses.
            let translator = match (hub.speed, speed) {
                (Speed::High, Speed::Low | Speed::Full) => Some(Translator { hub: hub.address, port }),
                (Speed::High, Speed::High) => None,
                _ => hub.translator,
            };
            self.device(speed, translator, depth + 1).map(Port::Device)
        });
        let found = found.unwrap_or_else(Port::Failed);
        if !matches!(found, Port::Device(_)) {
            // Out of the way of the next port's device at address 0. Nothing more to do if
            // the hub won't listen either.
            let _ = self.control(hub, SetupPacket::clear_port_feature(port, PortFeature::ENABLE), &mut []);
        }
        found
    }

    /// Looks at `device`'s ports, if it is a hub, and on down through the hubs on them, for
    /// devices that came or went.
    fn poll_hub(&mut self, device: &mut Device<B::Error>, depth: usize, changes: &mut Vec<Change>) {
        let target = device.target();
        let Some(hub) = device.hub.as_mut() else { return };
        for (number, port) in (1..).zip(hub.ports.iter_mut()) {
            // A hub that doesn't answer is going away itself: its own port will say so.
            let Ok(status) = self.port_status(target, number) else { continue };
            if !status.connection_changed() {
                if let Port::Device(child) = port {
                    self.poll_hub(child, depth + 1, changes);
                }
                continue;
            }
            let _ = self.control(target, SetupPacket::clear_port_feature(number, PortFeature::CONNECTION_CHANGE), &mut []);
            if let Port::Device(gone) = core::mem::replace(port, Port::Empty) {
                self.addresses.free(&gone);
                changes.push(Change::Removed { address: gone.address });
            }
            if status.connected() {
                *port = self.port(target, number, depth);
                if let Port::Device(added) = port {
                    changes.push(Change::Added { address: added.address });
                }
            }
        }
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
        /// The hub (by index) and port its transactions must be split through, if any.
        translator: Option<(usize, u8)>,
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
        connection_changed: bool,
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
            translator: None,
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
            devices[3].translator = Some((1, 2));
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
            status | (port.connection_changed as u32) << 16 | (port.reset_changed as u32) << 20
        }

        /// Pulls whatever is on `hub`'s `port` (counting from 1) out: it and everything behind
        /// it forget their addresses.
        fn unplug(&mut self, hub: usize, port: usize) {
            let fake_port = &mut self.devices[hub].hub.as_mut().unwrap().ports[port - 1];
            let Some(device) = fake_port.device.take() else { return };
            fake_port.enabled = false;
            fake_port.connection_changed = true;
            self.forget(device);
        }

        fn forget(&mut self, device: usize) {
            self.devices[device].address = None;
            self.devices[device].configured = None;
            let children: Vec<_> = self.devices[device].hub.iter().flat_map(|hub| hub.ports.iter().filter_map(|p| p.device)).collect();
            for child in children {
                self.forget(child);
            }
            if let Some(hub) = self.devices[device].hub.as_mut() {
                for port in &mut hub.ports {
                    port.powered = false;
                    port.enabled = false;
                }
            }
        }

        fn plug(&mut self, hub: usize, port: usize, device: usize) {
            let fake_port = &mut self.devices[hub].hub.as_mut().unwrap().ports[port - 1];
            fake_port.device = Some(device);
            fake_port.connection_changed = true;
        }
    }

    impl Bus for FakeBus {
        type Error = String;

        fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, String> {
            let index = self.find(target.address).ok_or("nobody at that address")?;
            let device = &self.devices[index];
            assert_eq!(target.speed, device.speed, "talking to a device at the wrong speed");
            let translator = device.translator.map(|(hub, port)| Translator { hub: self.devices[hub].address.unwrap(), port });
            assert_eq!(target.translator, translator, "split through the wrong translator");
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
                        16 => hub_port.connection_changed = false,
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
        let root = enumerate(&mut bus, Speed::High).unwrap().root;
        let found: Vec<_> = root.walk().iter().map(|(depth, d)| (*depth, d.address, d.descriptor.product)).collect();
        // Depth first: the second hub and what's on it come before the flash drive.
        assert_eq!(found, [(0, 1, 0x2514), (1, 2, 0x2514), (2, 3, 0x7800), (2, 4, 0xC31C), (1, 5, 0x5567)]);
        // Everyone found was configured.
        for index in 0..5 {
            assert_eq!(bus.devices[index].configured, Some(1));
        }
        let hub = root.hub.as_ref().unwrap();
        assert!(matches!(hub.ports[1], Port::Empty) && matches!(hub.ports[3], Port::Empty));
        assert!(bus.devices[0].hub.as_ref().unwrap().ports.iter().all(|port| port.powered));
    }

    #[test]
    fn slow_devices_are_reached_through_their_hubs_translator() {
        let mut bus = FakeBus::pi_3b_plus();
        let root = enumerate(&mut bus, Speed::High).unwrap().root;
        let second = root.hub.as_ref().unwrap().ports[0].device().expect("second hub");
        let keyboard = second.hub.as_ref().unwrap().ports[1].device().expect("keyboard");
        assert_eq!(keyboard.speed, Speed::Low);
        assert_eq!(keyboard.translator, Some(Translator { hub: second.address, port: 2 }));
        assert_eq!(keyboard.target().translator, keyboard.translator);
        // High speed devices need none, however deep.
        assert_eq!(second.hub.as_ref().unwrap().ports[0].device().unwrap().translator, None);
    }

    #[test]
    fn devices_behind_a_full_speed_hub_share_its_translator() {
        let mut bus = FakeBus::pi_3b_plus();
        // A full speed hub (5) on port 4 of the first hub, with a full speed device (6) on it.
        bus.devices.push(fake(Speed::Full, 9, 0x05E3, 0x0608, 2));
        bus.devices.push(fake(Speed::Full, 3, 0x1234, 0x0001, 0));
        bus.devices[0].hub.as_mut().unwrap().ports[3].device = Some(5);
        bus.devices[5].hub.as_mut().unwrap().ports[0].device = Some(6);
        bus.devices[5].translator = Some((0, 4));
        bus.devices[6].translator = Some((0, 4));
        let root = enumerate(&mut bus, Speed::High).unwrap().root;
        let hub = root.hub.as_ref().unwrap().ports[3].device().expect("full speed hub");
        let device = hub.hub.as_ref().unwrap().ports[0].device().expect("device behind it");
        assert_eq!(device.translator, Some(Translator { hub: 1, port: 4 }));
    }

    #[test]
    fn a_failing_device_is_reported_and_the_rest_still_found() {
        let mut bus = FakeBus::pi_3b_plus();
        bus.devices[2].broken = true; // the LAN7800
        let root = enumerate(&mut bus, Speed::High).unwrap().root;
        let Port::Device(second) = &root.hub.as_ref().unwrap().ports[0] else { panic!("second hub") };
        assert!(matches!(&second.hub.as_ref().unwrap().ports[0], Port::Failed(Error::Bus(message)) if message == "broken"));
        assert!(!bus.devices[1].hub.as_ref().unwrap().ports[0].enabled);
        // The broken device's address went back to be used again: the keyboard got it, and
        // the flash drive the next.
        let keyboard = second.hub.as_ref().unwrap().ports[1].device().expect("keyboard");
        assert_eq!(keyboard.address, 3);
        assert!(matches!(&root.hub.as_ref().unwrap().ports[2], Port::Device(drive) if drive.address == 4));
    }

    #[test]
    fn a_broken_root_device_is_an_error() {
        let mut bus = FakeBus::pi_3b_plus();
        bus.devices[0].broken = true;
        assert!(matches!(enumerate(&mut bus, Speed::High), Err(Error::Bus(_))));
    }

    #[test]
    fn nothing_changes_while_nothing_is_plugged_or_pulled() {
        let mut bus = FakeBus::pi_3b_plus();
        let mut tree = enumerate(&mut bus, Speed::High).unwrap();
        assert_eq!(tree.poll_changes(&mut bus), []);
        assert_eq!(tree.root.walk().len(), 5);
    }

    #[test]
    fn a_device_pulled_out_is_forgotten_and_found_again_when_plugged_back() {
        let mut bus = FakeBus::pi_3b_plus();
        let mut tree = enumerate(&mut bus, Speed::High).unwrap();
        bus.unplug(1, 2); // the keyboard
        assert_eq!(tree.poll_changes(&mut bus), [Change::Removed { address: 4 }]);
        assert!(tree.device(4).is_none());
        assert_eq!(tree.poll_changes(&mut bus), []);

        bus.plug(1, 2, 3);
        // It gets its old address back: the lowest free one.
        assert_eq!(tree.poll_changes(&mut bus), [Change::Added { address: 4 }]);
        let keyboard = tree.device(4).expect("keyboard back");
        assert_eq!(keyboard.descriptor.product, 0xC31C);
        assert_eq!(keyboard.translator, Some(Translator { hub: 2, port: 2 }));
        assert_eq!(bus.devices[3].configured, Some(1));
    }

    #[test]
    fn pulling_a_hub_takes_everything_behind_it() {
        let mut bus = FakeBus::pi_3b_plus();
        let mut tree = enumerate(&mut bus, Speed::High).unwrap();
        bus.unplug(0, 1); // the second hub, with the LAN7800 and the keyboard
        assert_eq!(tree.poll_changes(&mut bus), [Change::Removed { address: 2 }]);
        assert_eq!(tree.root.walk().len(), 2); // the first hub and the flash drive
        // Back again: the hub and all behind it are enumerated, reusing the freed addresses.
        bus.plug(0, 1, 1);
        assert_eq!(tree.poll_changes(&mut bus), [Change::Added { address: 2 }]);
        let found: Vec<_> = tree.root.walk().iter().map(|(_, d)| (d.address, d.descriptor.product)).collect();
        assert_eq!(found, [(1, 0x2514), (2, 0x2514), (3, 0x7800), (4, 0xC31C), (5, 0x5567)]);
    }

    #[test]
    fn a_device_swapped_between_looks_is_both_removed_and_added() {
        let mut bus = FakeBus::pi_3b_plus();
        let mut tree = enumerate(&mut bus, Speed::High).unwrap();
        // The flash drive out, and the keyboard moved into its port from the second hub.
        bus.unplug(0, 3);
        bus.unplug(1, 2);
        bus.plug(0, 3, 3);
        bus.devices[3].translator = Some((0, 3));
        let changes = tree.poll_changes(&mut bus);
        assert!(changes.contains(&Change::Removed { address: 5 }));
        assert!(changes.contains(&Change::Removed { address: 4 }));
        let moved = tree.root.hub.as_ref().unwrap().ports[2].device().expect("keyboard, moved");
        // Straight on the first hub now: its translator is that hub's.
        assert_eq!(moved.translator, Some(Translator { hub: 1, port: 3 }));
    }

    #[test]
    fn hubs_wait_for_their_ports_to_power_up() {
        let mut bus = FakeBus::pi_3b_plus();
        enumerate(&mut bus, Speed::High).unwrap();
        // Two hubs' power-up, at least `MIN_POWER_UP_MS` each, plus reset polling and recovery.
        assert!(bus.slept_ms >= 2 * MIN_POWER_UP_MS, "slept {} ms", bus.slept_ms);
    }
}
