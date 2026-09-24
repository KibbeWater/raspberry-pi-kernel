// usb/mod.rs
//! The USB protocol, independent of any host controller: control requests, the descriptors
//! devices describe themselves with, hub port status, and boot protocol keyboard reports.
//! See the USB 2.0 specification (chapters 9 and 11) and the HID 1.11 specification
//! (appendix B).

pub mod tree;

use alloc::vec::Vec;
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Host to device.
    Out,
    /// Device to host.
    In,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

impl EndpointType {
    fn from_attributes(attributes: u8) -> Self {
        match attributes & 0b11 {
            0 => EndpointType::Control,
            1 => EndpointType::Isochronous,
            2 => EndpointType::Bulk,
            _ => EndpointType::Interrupt,
        }
    }
}

/// A descriptor's type, its second byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorType(pub u8);

impl DescriptorType {
    pub const DEVICE: DescriptorType = DescriptorType(1);
    pub const CONFIGURATION: DescriptorType = DescriptorType(2);
    pub const STRING: DescriptorType = DescriptorType(3);
    pub const INTERFACE: DescriptorType = DescriptorType(4);
    pub const ENDPOINT: DescriptorType = DescriptorType(5);
    pub const HID: DescriptorType = DescriptorType(0x21);
    pub const HUB: DescriptorType = DescriptorType(0x29);
}

/// Device and interface classes this kernel knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class(pub u8);

impl Class {
    /// Given per interface instead.
    pub const PER_INTERFACE: Class = Class(0);
    pub const HID: Class = Class(3);
    pub const HUB: Class = Class(9);
    pub const VENDOR: Class = Class(0xFF);

    /// What the class is, for people.
    pub fn name(self) -> &'static str {
        match self.0 {
            0x00 => "per interface",
            0x01 => "audio",
            0x02 => "communications",
            0x03 => "HID",
            0x06 => "imaging",
            0x07 => "printer",
            0x08 => "mass storage",
            0x09 => "hub",
            0x0A => "CDC data",
            0x0B => "smart card",
            0x0E => "video",
            0xE0 => "wireless",
            0xEF => "miscellaneous",
            0xFF => "vendor specific",
            _ => "other",
        }
    }
}

/// HID interface subclass and protocol of a keyboard that speaks the boot protocol.
pub const HID_SUBCLASS_BOOT: u8 = 1;
pub const HID_PROTOCOL_KEYBOARD: u8 = 1;

/// The 8 bytes that start every control transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    /// Bytes in the data stage.
    pub length: u16,
}

/// `request_type` fields: direction, kind of request, and whom it is for.
const TYPE_IN: u8 = 0x80;
const TYPE_CLASS: u8 = 1 << 5;
const TYPE_VENDOR: u8 = 2 << 5;
const TO_INTERFACE: u8 = 1;
const TO_OTHER: u8 = 3;

// Standard requests.
const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const SET_ADDRESS: u8 = 5;
const GET_DESCRIPTOR: u8 = 6;
const SET_CONFIGURATION: u8 = 9;
// HID class requests.
const SET_IDLE: u8 = 0x0A;
const SET_PROTOCOL: u8 = 0x0B;

/// Hub port features, for `set_port_feature` and `clear_port_feature`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortFeature(pub u16);

impl PortFeature {
    /// Only cleared, to disable a port.
    pub const ENABLE: PortFeature = PortFeature(1);
    pub const RESET: PortFeature = PortFeature(4);
    pub const POWER: PortFeature = PortFeature(8);
    pub const CONNECTION_CHANGE: PortFeature = PortFeature(16);
    pub const ENABLE_CHANGE: PortFeature = PortFeature(17);
    pub const RESET_CHANGE: PortFeature = PortFeature(20);
}

impl SetupPacket {
    pub fn direction(&self) -> Direction {
        if self.request_type & TYPE_IN != 0 { Direction::In } else { Direction::Out }
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let [value_lo, value_hi] = self.value.to_le_bytes();
        let [index_lo, index_hi] = self.index.to_le_bytes();
        let [length_lo, length_hi] = self.length.to_le_bytes();
        [self.request_type, self.request, value_lo, value_hi, index_lo, index_hi, length_lo, length_hi]
    }

    /// Up to `length` bytes of the device's descriptor of type `kind` (the first of them,
    /// `index` 0, for most kinds).
    pub fn get_descriptor(kind: DescriptorType, index: u8, length: u16) -> Self {
        SetupPacket { request_type: TYPE_IN, request: GET_DESCRIPTOR, value: (kind.0 as u16) << 8 | index as u16, index: 0, length }
    }

    /// Gives the device, still at address 0, its own address.
    pub fn set_address(address: u8) -> Self {
        SetupPacket { request_type: 0, request: SET_ADDRESS, value: address as u16, index: 0, length: 0 }
    }

    pub fn set_configuration(value: u8) -> Self {
        SetupPacket { request_type: 0, request: SET_CONFIGURATION, value: value as u16, index: 0, length: 0 }
    }

    /// A hub's own descriptor (a class descriptor, so not `get_descriptor`).
    pub fn get_hub_descriptor(length: u16) -> Self {
        SetupPacket {
            request_type: TYPE_IN | TYPE_CLASS,
            request: GET_DESCRIPTOR,
            value: (DescriptorType::HUB.0 as u16) << 8,
            index: 0,
            length,
        }
    }

    /// A hub port's status and change bits (see `PortStatus`), 4 bytes. Ports count from 1.
    pub fn get_port_status(port: u8) -> Self {
        SetupPacket { request_type: TYPE_IN | TYPE_CLASS | TO_OTHER, request: GET_STATUS, value: 0, index: port as u16, length: 4 }
    }

    pub fn set_port_feature(port: u8, feature: PortFeature) -> Self {
        SetupPacket { request_type: TYPE_CLASS | TO_OTHER, request: SET_FEATURE, value: feature.0, index: port as u16, length: 0 }
    }

    pub fn clear_port_feature(port: u8, feature: PortFeature) -> Self {
        SetupPacket { request_type: TYPE_CLASS | TO_OTHER, request: CLEAR_FEATURE, value: feature.0, index: port as u16, length: 0 }
    }

    /// Switches a HID interface to the boot protocol (`boot`), or back to its report protocol.
    pub fn set_protocol(interface: u8, boot: bool) -> Self {
        SetupPacket {
            request_type: TYPE_CLASS | TO_INTERFACE,
            request: SET_PROTOCOL,
            value: if boot { 0 } else { 1 },
            index: interface as u16,
            length: 0,
        }
    }

    /// Asks a HID interface to report only when something changes.
    pub fn set_idle(interface: u8) -> Self {
        SetupPacket { request_type: TYPE_CLASS | TO_INTERFACE, request: SET_IDLE, value: 0, index: interface as u16, length: 0 }
    }

    /// A vendor-specific request, like the LAN7800's register reads and writes.
    pub fn vendor(direction: Direction, request: u8, value: u16, index: u16, length: u16) -> Self {
        let dir = if direction == Direction::In { TYPE_IN } else { 0 };
        SetupPacket { request_type: dir | TYPE_VENDOR, request, value, index, length }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorError {
    /// Shorter than its type needs, or than it says it is.
    Truncated,
    /// Not the type asked for.
    WrongType(DescriptorType),
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DescriptorError::Truncated => write!(f, "descriptor cut short"),
            DescriptorError::WrongType(kind) => write!(f, "unexpected descriptor type {:#x}", kind.0),
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// Checks `bytes` starts with a descriptor of type `kind` at least `min` bytes long.
fn check(bytes: &[u8], kind: DescriptorType, min: usize) -> Result<(), DescriptorError> {
    if bytes.len() < 2 || bytes.len() < min || (bytes[0] as usize) < min {
        return Err(DescriptorError::Truncated);
    }
    if DescriptorType(bytes[1]) != kind {
        return Err(DescriptorError::WrongType(DescriptorType(bytes[1])));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub class: Class,
    pub subclass: u8,
    pub protocol: u8,
    /// Of endpoint 0. The first 8 bytes of the descriptor, which say it, can always be read
    /// with packets of 8.
    pub max_packet_size: u8,
    pub vendor: u16,
    pub product: u16,
    pub configurations: u8,
}

impl DeviceDescriptor {
    pub const LENGTH: usize = 18;

    /// From the first 8 bytes: just enough for `max_packet_size`.
    pub fn max_packet_size(bytes: &[u8]) -> Result<u8, DescriptorError> {
        check(bytes, DescriptorType::DEVICE, 8)?;
        Ok(bytes[7])
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, DescriptorError> {
        check(bytes, DescriptorType::DEVICE, Self::LENGTH)?;
        Ok(DeviceDescriptor {
            class: Class(bytes[4]),
            subclass: bytes[5],
            protocol: bytes[6],
            max_packet_size: bytes[7],
            vendor: u16_at(bytes, 8),
            product: u16_at(bytes, 10),
            configurations: bytes[17],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// 1 to 15.
    pub number: u8,
    pub direction: Direction,
    pub kind: EndpointType,
    pub max_packet_size: u16,
    /// How often to poll it, for interrupt endpoints: in frames (ms) at low and full speed,
    /// as a power of two of microframes at high speed.
    pub interval: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub alternate: u8,
    pub class: Class,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
}

impl Interface {
    pub fn is_boot_keyboard(&self) -> bool {
        self.class == Class::HID && self.subclass == HID_SUBCLASS_BOOT && self.protocol == HID_PROTOCOL_KEYBOARD
    }

    pub fn endpoint(&self, kind: EndpointType, direction: Direction) -> Option<Endpoint> {
        self.endpoints.iter().copied().find(|e| e.kind == kind && e.direction == direction)
    }
}

/// A configuration, with its interfaces and their endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    /// What `set_configuration` takes to choose it.
    pub value: u8,
    pub interfaces: Vec<Interface>,
}

impl Configuration {
    const HEADER: usize = 9;

    /// From the first 9 bytes: how many bytes the whole configuration takes, to ask for it
    /// all.
    pub fn total_length(bytes: &[u8]) -> Result<u16, DescriptorError> {
        check(bytes, DescriptorType::CONFIGURATION, Self::HEADER)?;
        Ok(u16_at(bytes, 2))
    }

    /// The whole configuration, as `total_length` gives. Descriptors of other kinds (HID, and
    /// class or vendor specific ones) are passed over.
    pub fn parse(bytes: &[u8]) -> Result<Self, DescriptorError> {
        let total = Self::total_length(bytes)? as usize;
        if bytes.len() < total {
            return Err(DescriptorError::Truncated);
        }
        let mut configuration = Configuration { value: bytes[5], interfaces: Vec::new() };
        let mut at = bytes[0] as usize;
        while at + 2 <= total {
            let length = bytes[at] as usize;
            if length < 2 || at + length > total {
                return Err(DescriptorError::Truncated);
            }
            let descriptor = &bytes[at..at + length];
            match DescriptorType(descriptor[1]) {
                DescriptorType::INTERFACE => {
                    check(descriptor, DescriptorType::INTERFACE, 9)?;
                    configuration.interfaces.push(Interface {
                        number: descriptor[2],
                        alternate: descriptor[3],
                        class: Class(descriptor[5]),
                        subclass: descriptor[6],
                        protocol: descriptor[7],
                        endpoints: Vec::new(),
                    });
                }
                DescriptorType::ENDPOINT => {
                    check(descriptor, DescriptorType::ENDPOINT, 7)?;
                    let endpoint = Endpoint {
                        number: descriptor[2] & 0x0F,
                        direction: if descriptor[2] & 0x80 != 0 { Direction::In } else { Direction::Out },
                        kind: EndpointType::from_attributes(descriptor[3]),
                        max_packet_size: u16_at(descriptor, 4) & 0x7FF,
                        interval: descriptor[6],
                    };
                    // An endpoint belongs to the interface before it.
                    if let Some(interface) = configuration.interfaces.last_mut() {
                        interface.endpoints.push(endpoint);
                    }
                }
                _ => {}
            }
            at += length;
        }
        Ok(configuration)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDescriptor {
    pub ports: u8,
    /// How long after powering a port it can be used.
    pub power_on_to_good_ms: u32,
}

impl HubDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self, DescriptorError> {
        check(bytes, DescriptorType::HUB, 7)?;
        Ok(HubDescriptor { ports: bytes[2], power_on_to_good_ms: bytes[5] as u32 * 2 })
    }
}

/// A hub port's status and what changed, from `get_port_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus(pub u32);

impl PortStatus {
    pub fn from_bytes(bytes: [u8; 4]) -> Self {
        PortStatus(u32::from_le_bytes(bytes))
    }

    fn bit(self, n: u32) -> bool {
        self.0 & 1 << n != 0
    }

    pub fn connected(self) -> bool {
        self.bit(0)
    }

    pub fn enabled(self) -> bool {
        self.bit(1)
    }

    pub fn overcurrent(self) -> bool {
        self.bit(3)
    }

    pub fn resetting(self) -> bool {
        self.bit(4)
    }

    pub fn powered(self) -> bool {
        self.bit(8)
    }

    /// The speed of the device on it, once it is enabled.
    pub fn speed(self) -> Speed {
        if self.bit(9) {
            Speed::Low
        } else if self.bit(10) {
            Speed::High
        } else {
            Speed::Full
        }
    }

    pub fn connection_changed(self) -> bool {
        self.bit(16)
    }

    pub fn reset_changed(self) -> bool {
        self.bit(20)
    }
}

/// What a boot protocol keyboard reports on its interrupt endpoint: which modifiers are held,
/// and up to six other keys, by HID usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardReport {
    pub modifiers: u8,
    pub keys: [u8; 6],
}

/// Modifier bits: left control, shift, alt, GUI, then the right ones.
const SHIFT: u8 = 1 << 1 | 1 << 5;
/// A key usage meaning "too many keys held to tell".
const ROLLOVER: u8 = 1;

impl KeyboardReport {
    pub const LENGTH: usize = 8;

    pub fn parse(bytes: &[u8]) -> Result<Self, DescriptorError> {
        if bytes.len() < Self::LENGTH {
            return Err(DescriptorError::Truncated);
        }
        let mut keys = [0; 6];
        keys.copy_from_slice(&bytes[2..8]);
        Ok(KeyboardReport { modifiers: bytes[0], keys })
    }

    pub fn shift(&self) -> bool {
        self.modifiers & SHIFT != 0
    }

    /// Keys held now that weren't in `previous`, the report before: the ones just pressed.
    pub fn pressed_since<'a>(&'a self, previous: &'a KeyboardReport) -> impl Iterator<Item = u8> + 'a {
        let rollover = self.keys.contains(&ROLLOVER);
        self.keys.iter().copied().filter(move |&key| !rollover && key > ROLLOVER && !previous.keys.contains(&key))
    }
}

/// The character a key gives on a US layout, with shift held or not, if it gives one.
pub fn key_to_char(usage: u8, shift: bool) -> Option<char> {
    const LETTERS: core::ops::RangeInclusive<u8> = 0x04..=0x1D;
    const DIGITS: &[u8; 10] = b"1234567890";
    const SHIFTED_DIGITS: &[u8; 10] = b"!@#$%^&*()";
    // Usages 0x2C to 0x38: space and punctuation.
    const PUNCTUATION: &[u8; 13] = b" -=[]\\#;'`,./";
    const SHIFTED_PUNCTUATION: &[u8; 13] = b" _+{}|~:\"~<>?";
    let byte = match usage {
        usage if LETTERS.contains(&usage) => {
            let letter = b'a' + (usage - 0x04);
            if shift { letter.to_ascii_uppercase() } else { letter }
        }
        0x1E..=0x27 => {
            let i = (usage - 0x1E) as usize;
            if shift { SHIFTED_DIGITS[i] } else { DIGITS[i] }
        }
        0x28 => b'\n',
        0x2A => 0x08, // backspace
        0x2B => b'\t',
        0x2C..=0x38 => {
            let i = (usage - 0x2C) as usize;
            if shift { SHIFTED_PUNCTUATION[i] } else { PUNCTUATION[i] }
        }
        _ => return None,
    };
    Some(byte as char)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical low-speed boot keyboard's device descriptor.
    const KEYBOARD_DEVICE: [u8; 18] = [
        18, 1, 0x10, 0x01, 0, 0, 0, 8, 0x6D, 0x04, 0x1C, 0xC3, 0x00, 0x01, 1, 2, 0, 1,
    ];

    /// Its configuration: one boot keyboard interface, a HID descriptor, an interrupt IN
    /// endpoint polled every 10ms.
    const KEYBOARD_CONFIGURATION: [u8; 34] = [
        9, 2, 34, 0, 1, 1, 0, 0xA0, 50, // configuration
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface 0: HID, boot, keyboard
        9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, // HID
        7, 5, 0x81, 3, 8, 0, 10, // endpoint 1 IN, interrupt, 8 bytes, 10ms
    ];

    /// A 4-port hub's descriptor, like the first hub in the Pi 3 B+'s LAN7515.
    const HUB: [u8; 9] = [9, 0x29, 4, 0x09, 0, 50, 0, 0, 0xFF];

    #[test]
    fn setup_packets_encode_little_endian() {
        let packet = SetupPacket::get_descriptor(DescriptorType::CONFIGURATION, 0, 0x0122);
        assert_eq!(packet.to_bytes(), [0x80, 6, 0, 2, 0, 0, 0x22, 0x01]);
        assert_eq!(packet.direction(), Direction::In);
        assert_eq!(SetupPacket::set_address(5).to_bytes(), [0, 5, 5, 0, 0, 0, 0, 0]);
        assert_eq!(SetupPacket::set_address(5).direction(), Direction::Out);
    }

    #[test]
    fn hub_and_hid_requests_go_to_the_right_recipient() {
        assert_eq!(SetupPacket::get_hub_descriptor(9).to_bytes(), [0xA0, 6, 0, 0x29, 0, 0, 9, 0]);
        assert_eq!(SetupPacket::get_port_status(2).to_bytes(), [0xA3, 0, 0, 0, 2, 0, 4, 0]);
        assert_eq!(SetupPacket::set_port_feature(3, PortFeature::RESET).to_bytes(), [0x23, 3, 4, 0, 3, 0, 0, 0]);
        assert_eq!(SetupPacket::clear_port_feature(1, PortFeature::RESET_CHANGE).to_bytes(), [0x23, 1, 20, 0, 1, 0, 0, 0]);
        assert_eq!(SetupPacket::set_protocol(0, true).to_bytes(), [0x21, 0x0B, 0, 0, 0, 0, 0, 0]);
        assert_eq!(SetupPacket::set_idle(1).to_bytes(), [0x21, 0x0A, 0, 0, 1, 0, 0, 0]);
        assert_eq!(SetupPacket::vendor(Direction::In, 0xA1, 0, 0x10, 4).to_bytes(), [0xC0, 0xA1, 0, 0, 0x10, 0, 4, 0]);
    }

    #[test]
    fn device_descriptors_parse() {
        assert_eq!(DeviceDescriptor::max_packet_size(&KEYBOARD_DEVICE[..8]), Ok(8));
        let device = DeviceDescriptor::parse(&KEYBOARD_DEVICE).unwrap();
        assert_eq!((device.vendor, device.product), (0x046D, 0xC31C));
        assert_eq!(device.class, Class::PER_INTERFACE);
        assert_eq!(device.configurations, 1);
        assert_eq!(DeviceDescriptor::parse(&KEYBOARD_DEVICE[..12]), Err(DescriptorError::Truncated));
        assert_eq!(DeviceDescriptor::parse(&HUB), Err(DescriptorError::Truncated));
        let mut wrong = KEYBOARD_DEVICE;
        wrong[1] = 2;
        assert_eq!(DeviceDescriptor::parse(&wrong), Err(DescriptorError::WrongType(DescriptorType::CONFIGURATION)));
    }

    #[test]
    fn configurations_parse_with_their_interfaces_and_endpoints() {
        assert_eq!(Configuration::total_length(&KEYBOARD_CONFIGURATION[..9]), Ok(34));
        let configuration = Configuration::parse(&KEYBOARD_CONFIGURATION).unwrap();
        assert_eq!(configuration.value, 1);
        let [interface] = configuration.interfaces.as_slice() else { panic!("one interface") };
        assert!(interface.is_boot_keyboard());
        let endpoint = interface.endpoint(EndpointType::Interrupt, Direction::In).unwrap();
        assert_eq!((endpoint.number, endpoint.max_packet_size, endpoint.interval), (1, 8, 10));
        assert_eq!(interface.endpoint(EndpointType::Bulk, Direction::In), None);
    }

    #[test]
    fn broken_configurations_are_errors() {
        assert_eq!(Configuration::parse(&KEYBOARD_CONFIGURATION[..20]), Err(DescriptorError::Truncated));
        let mut zero_length = KEYBOARD_CONFIGURATION;
        zero_length[9] = 0;
        assert_eq!(Configuration::parse(&zero_length), Err(DescriptorError::Truncated));
        let mut overlong = KEYBOARD_CONFIGURATION;
        overlong[27] = 20;
        assert_eq!(Configuration::parse(&overlong), Err(DescriptorError::Truncated));
    }

    #[test]
    fn hub_descriptors_and_port_status_parse() {
        let hub = HubDescriptor::parse(&HUB).unwrap();
        assert_eq!((hub.ports, hub.power_on_to_good_ms), (4, 100));
        // Connected, enabled, powered, high speed; connection changed.
        let status = PortStatus::from_bytes([0x03, 0x05, 0x01, 0x00]);
        assert!(status.connected() && status.enabled() && status.powered());
        assert_eq!(status.speed(), Speed::High);
        assert!(status.connection_changed() && !status.reset_changed());
        assert_eq!(PortStatus(1 << 9).speed(), Speed::Low);
        assert_eq!(PortStatus(0).speed(), Speed::Full);
    }

    #[test]
    fn keyboard_reports_give_newly_pressed_keys() {
        let idle = KeyboardReport::parse(&[0; 8]).unwrap();
        // Shift and 'a' held.
        let a = KeyboardReport::parse(&[0x02, 0, 0x04, 0, 0, 0, 0, 0]).unwrap();
        assert!(a.shift());
        assert_eq!(a.pressed_since(&idle).collect::<Vec<_>>(), [0x04]);
        // 'b' joins while 'a' stays held: only 'b' is new.
        let ab = KeyboardReport::parse(&[0, 0, 0x04, 0x05, 0, 0, 0, 0]).unwrap();
        assert_eq!(ab.pressed_since(&a).collect::<Vec<_>>(), [0x05]);
        // Too many keys: nothing can be told apart.
        let rollover = KeyboardReport::parse(&[0, 0, 1, 1, 1, 1, 1, 1]).unwrap();
        assert_eq!(rollover.pressed_since(&idle).count(), 0);
        assert_eq!(KeyboardReport::parse(&[0; 4]), Err(DescriptorError::Truncated));
    }

    #[test]
    fn keys_map_to_us_layout_characters() {
        assert_eq!(key_to_char(0x04, false), Some('a'));
        assert_eq!(key_to_char(0x1D, true), Some('Z'));
        assert_eq!(key_to_char(0x1E, false), Some('1'));
        assert_eq!(key_to_char(0x1F, true), Some('@'));
        assert_eq!(key_to_char(0x27, false), Some('0'));
        assert_eq!(key_to_char(0x28, false), Some('\n'));
        assert_eq!(key_to_char(0x2C, true), Some(' '));
        assert_eq!(key_to_char(0x2D, true), Some('_'));
        assert_eq!(key_to_char(0x38, false), Some('/'));
        assert_eq!(key_to_char(0x38, true), Some('?'));
        assert_eq!(key_to_char(0x3A, false), None); // F1
    }
}
