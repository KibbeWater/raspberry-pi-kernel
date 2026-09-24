// lan7800.rs
//! The Microchip LAN7800 USB gigabit Ethernet controller, on the Pi 3 B+'s USB bus behind the
//! LAN7515's hubs. Its registers are read and written with vendor control requests; frames
//! go out on a bulk OUT endpoint and come in on a bulk IN one, wrapped as
//! `rustypi_core::net::lan7800` describes. Register names and the bring-up order follow
//! Circle's driver (<https://github.com/rsta2/circle>, `lib/usb/lan7800.cpp`).

use alloc::vec::Vec;
use core::fmt;
use rustypi_core::net::{lan7800, Mac};
use rustypi_core::usb::tree::{Bus, Device, Target};
use rustypi_core::usb::{Direction, EndpointType, SetupPacket};
use crate::drivers::timer;
use crate::drivers::usb::{BulkEndpoint, Host, Toggle, UsbError};

pub const VENDOR: u16 = 0x0424;
pub const PRODUCT: u16 = 0x7800;

const WRITE_REGISTER: u8 = 0xA0;
const READ_REGISTER: u8 = 0xA1;

// Registers, and their bits.
const ID_REV: u16 = 0x000;
const INT_STS: u16 = 0x00C;
const HW_CFG: u16 = 0x010;
const HW_CFG_LED1_EN: u32 = 1 << 21;
const HW_CFG_LED0_EN: u32 = 1 << 20;
const HW_CFG_MEF: u32 = 1 << 4;
const HW_CFG_LRST: u32 = 1 << 1;
const PMT_CTL: u16 = 0x014;
const PMT_CTL_READY: u32 = 1 << 7;
const PMT_CTL_PHY_RST: u32 = 1 << 4;
const USB_CFG0: u16 = 0x080;
/// Bulk IN empty response: NAK when set, a zero-length packet when clear.
const USB_CFG_BIR: u32 = 1 << 6;
/// Burst cap enable.
const USB_CFG_BCE: u32 = 1 << 5;
const BURST_CAP: u16 = 0x090;
const BULK_IN_DLY: u16 = 0x094;
const INT_EP_CTL: u16 = 0x098;
const RFE_CTL: u16 = 0x0B0;
const RFE_CTL_BCAST_EN: u32 = 1 << 10;
const RFE_CTL_DA_PERFECT: u32 = 1 << 1;
const FCT_RX_CTL: u16 = 0x0C0;
const FCT_TX_CTL: u16 = 0x0C4;
const FCT_CTL_EN: u32 = 1 << 31;
const FCT_RX_FIFO_END: u16 = 0x0C8;
const FCT_TX_FIFO_END: u16 = 0x0CC;
const FCT_FLOW: u16 = 0x0D0;
const MAC_CR: u16 = 0x100;
const MAC_CR_AUTO_DUPLEX: u32 = 1 << 12;
const MAC_CR_AUTO_SPEED: u32 = 1 << 11;
const MAC_RX: u16 = 0x104;
const MAC_RX_MAX_SIZE_SHIFT: u32 = 16;
const MAC_RX_MAX_SIZE_MASK: u32 = 0x3FFF_0000;
const MAC_RX_RXEN: u32 = 1 << 0;
const MAC_TX: u16 = 0x108;
const MAC_TX_TXEN: u32 = 1 << 0;
const FLOW: u16 = 0x10C;
const RX_ADDRH: u16 = 0x118;
const RX_ADDRL: u16 = 0x11C;
const MII_ACC: u16 = 0x120;
const MII_ACC_PHY_ADDRESS: u32 = 1 << 11;
const MII_ACC_WRITE: u32 = 1 << 1;
const MII_ACC_BUSY: u32 = 1 << 0;
const MII_DATA: u16 = 0x124;
/// The perfect address filter: entry 0 is our own address.
const MAF_HI_0: u16 = 0x400;
const MAF_LO_0: u16 = 0x404;
const MAF_HI_VALID: u32 = 1 << 31;

/// FIFOs of 12KB each way.
const FIFO_SIZE: u32 = 12 * 1024;
/// A burst on the bulk IN endpoint is at most this, in 512-byte packets: what one bulk IN
/// transfer takes.
const BURST_BYTES: u32 = 4096;
const HS_PACKET: u32 = 512;
const BULK_IN_DELAY: u32 = 0x800;
/// Biggest frame received: two addresses, type, 1500 bytes of payload, FCS.
const MAX_RX_FRAME: u32 = 6 + 6 + 2 + 1500 + 4;
/// Biggest frame sent, without the controller's FCS.
pub const MAX_FRAME: usize = 1514;

/// PHY registers: basic status, LED mode, and (on page 0) auxiliary status.
const PHY_STATUS: u8 = 0x01;
const PHY_STATUS_LINK: u16 = 1 << 2;
const PHY_LED_MODE: u8 = 0x1D;
const PHY_AUX_STATUS: u8 = 0x1C;
const PHY_PAGE: u8 = 0x1F;

const REGISTER_TIMEOUT_US: u64 = 1_000_000;

#[derive(Debug)]
pub enum Lan7800Error {
    Usb(UsbError),
    /// The ID register isn't a LAN7800's.
    NotLan7800(u32),
    /// It has no bulk endpoints to move frames on.
    NoEndpoints,
    Timeout(&'static str),
    FrameTooBig,
}

impl From<UsbError> for Lan7800Error {
    fn from(error: UsbError) -> Self {
        Lan7800Error::Usb(error)
    }
}

impl fmt::Display for Lan7800Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Lan7800Error::Usb(error) => write!(f, "{error}"),
            Lan7800Error::NotLan7800(id) => write!(f, "not a LAN7800 (id {id:#x})"),
            Lan7800Error::NoEndpoints => write!(f, "no bulk endpoints"),
            Lan7800Error::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Lan7800Error::FrameTooBig => write!(f, "frame too big"),
        }
    }
}

/// The link's state, as the PHY has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    /// Once autonegotiation is done: megabits a second, and whether full duplex.
    pub speed: Option<(u16, bool)>,
}

pub struct Lan7800 {
    device: Target,
    rx: BulkEndpoint,
    tx: BulkEndpoint,
    rx_toggle: Toggle,
    tx_toggle: Toggle,
    pub mac: Mac,
}

impl Lan7800 {
    /// Brings the controller at `device` up with MAC address `mac`: reset, filters, FIFOs,
    /// PHY and autonegotiation, receiving and sending on.
    pub fn start(host: &mut Host, device: &Device<UsbError>, mac: Mac) -> Result<Lan7800, Lan7800Error> {
        let interface = device.configuration.interfaces.first().ok_or(Lan7800Error::NoEndpoints)?;
        let rx = interface.endpoint(EndpointType::Bulk, Direction::In).ok_or(Lan7800Error::NoEndpoints)?;
        let tx = interface.endpoint(EndpointType::Bulk, Direction::Out).ok_or(Lan7800Error::NoEndpoints)?;
        let target = device.target();
        let mut lan = Lan7800 {
            device: target,
            rx: BulkEndpoint { target, endpoint: rx.number, max_packet: rx.max_packet_size },
            tx: BulkEndpoint { target, endpoint: tx.number, max_packet: tx.max_packet_size },
            rx_toggle: Toggle::new(),
            tx_toggle: Toggle::new(),
            mac,
        };
        lan.init(host)?;
        Ok(lan)
    }

    fn init(&mut self, host: &mut Host) -> Result<(), Lan7800Error> {
        let id = self.read(host, ID_REV)?;
        if id >> 16 != 0x7800 {
            return Err(Lan7800Error::NotLan7800(id));
        }
        self.modify(host, HW_CFG, HW_CFG_LRST, !0)?;
        self.wait(host, HW_CFG, HW_CFG_LRST, 0, "a reset")?;

        // Our address, and a perfect filter entry for it.
        let [a, b, c, d, e, f] = self.mac.0;
        let low = u32::from_le_bytes([a, b, c, d]);
        let high = u16::from_le_bytes([e, f]) as u32;
        self.write(host, RX_ADDRL, low)?;
        self.write(host, RX_ADDRH, high)?;
        self.write(host, MAF_LO_0, low)?;
        self.write(host, MAF_HI_0, high | MAF_HI_VALID)?;

        self.write(host, BURST_CAP, BURST_BYTES / HS_PACKET)?;
        self.write(host, BULK_IN_DLY, BULK_IN_DELAY)?;
        // LEDs on, single frames per transfer off (so bursts can carry several).
        self.modify(host, HW_CFG, HW_CFG_LED0_EN | HW_CFG_LED1_EN, !HW_CFG_MEF)?;
        // Bursts capped; an empty receive FIFO answers with a zero-length packet, not a NAK,
        // so a poll with nothing to receive returns straight away.
        self.modify(host, USB_CFG0, USB_CFG_BCE, !USB_CFG_BIR)?;
        self.write(host, FCT_RX_FIFO_END, (FIFO_SIZE - 512) / 512)?;
        self.write(host, FCT_TX_FIFO_END, (FIFO_SIZE - 512) / 512)?;
        // No interrupt endpoint, no flow control.
        self.write(host, INT_EP_CTL, 0)?;
        self.write(host, INT_STS, u32::MAX)?;
        self.write(host, FLOW, 0)?;
        self.write(host, FCT_FLOW, 0)?;
        // Take broadcasts and frames for our address.
        self.modify(host, RFE_CTL, RFE_CTL_BCAST_EN | RFE_CTL_DA_PERFECT, !0)?;

        self.modify(host, PMT_CTL, PMT_CTL_PHY_RST, !0)?;
        self.wait(host, PMT_CTL, PMT_CTL_PHY_RST | PMT_CTL_READY, PMT_CTL_READY, "the PHY to reset")?;
        self.modify(host, MAC_CR, MAC_CR_AUTO_DUPLEX | MAC_CR_AUTO_SPEED, !0)?;

        self.modify(host, MAC_TX, MAC_TX_TXEN, !0)?;
        self.modify(host, FCT_TX_CTL, FCT_CTL_EN, !0)?;
        self.modify(host, MAC_RX, MAX_RX_FRAME << MAC_RX_MAX_SIZE_SHIFT | MAC_RX_RXEN, !MAC_RX_MAX_SIZE_MASK)?;
        self.modify(host, FCT_RX_CTL, FCT_CTL_EN, !0)?;

        // LEDs: green for link and activity at 10/100/1000, orange for gigabit.
        self.phy_write(host, PHY_PAGE, 0)?;
        let leds = self.phy_read(host, PHY_LED_MODE)?;
        self.phy_write(host, PHY_LED_MODE, (leds & !0xFF) | 1 | 6 << 4)?;
        Ok(())
    }

    /// Whether the cable is in and the link up, and at what speed.
    pub fn link(&self, host: &mut Host) -> Result<Link, Lan7800Error> {
        let up = self.phy_read(host, PHY_STATUS)? & PHY_STATUS_LINK != 0;
        self.phy_write(host, PHY_PAGE, 0)?;
        let aux = self.phy_read(host, PHY_AUX_STATUS)?;
        // Bit 15: autonegotiation done. Bits 5:3: the speed and duplex it settled on.
        let negotiated = up && aux & 1 << 15 != 0;
        let speed = match aux >> 3 & 7 {
            _ if !negotiated => None,
            0b000 => Some((10, false)),
            0b001 => Some((100, false)),
            0b010 => Some((1000, false)),
            0b100 => Some((10, true)),
            0b101 => Some((100, true)),
            0b110 => Some((1000, true)),
            _ => None,
        };
        Ok(Link { up, speed })
    }

    /// Sends an Ethernet frame (without its FCS, which the controller adds).
    pub fn send(&mut self, host: &mut Host, frame: &[u8]) -> Result<(), Lan7800Error> {
        if frame.len() > MAX_FRAME {
            return Err(Lan7800Error::FrameTooBig);
        }
        host.bulk_out(self.tx, &mut self.tx_toggle, &lan7800::tx(frame))?;
        Ok(())
    }

    /// The frames that have arrived since the last call, if any.
    pub fn receive(&mut self, host: &mut Host) -> Result<Vec<Vec<u8>>, Lan7800Error> {
        let transfer = host.bulk_in(self.rx, &mut self.rx_toggle)?;
        Ok(lan7800::rx(transfer).into_iter().map(|frame| frame.to_vec()).collect())
    }

    fn read(&self, host: &mut Host, register: u16) -> Result<u32, Lan7800Error> {
        let mut value = [0; 4];
        host.control(self.device, SetupPacket::vendor(Direction::In, READ_REGISTER, 0, register, 4), &mut value)?;
        Ok(u32::from_le_bytes(value))
    }

    fn write(&self, host: &mut Host, register: u16, value: u32) -> Result<(), Lan7800Error> {
        let mut bytes = value.to_le_bytes();
        host.control(self.device, SetupPacket::vendor(Direction::Out, WRITE_REGISTER, 0, register, 4), &mut bytes)?;
        Ok(())
    }

    /// Reads `register`, keeps the bits in `and`, sets those in `or`, and writes it back.
    fn modify(&self, host: &mut Host, register: u16, or: u32, and: u32) -> Result<(), Lan7800Error> {
        let value = self.read(host, register)?;
        self.write(host, register, value & and | or)
    }

    /// Waits for `register`'s bits in `mask` to read `expected`.
    fn wait(&self, host: &mut Host, register: u16, mask: u32, expected: u32, what: &'static str) -> Result<(), Lan7800Error> {
        let started = timer::now_us();
        while self.read(host, register)? & mask != expected {
            if timer::now_us() - started > REGISTER_TIMEOUT_US {
                return Err(Lan7800Error::Timeout(what));
            }
        }
        Ok(())
    }

    fn phy_access(&self, host: &mut Host, index: u8, write: bool) -> Result<(), Lan7800Error> {
        let direction = if write { MII_ACC_WRITE } else { 0 };
        self.write(host, MII_ACC, MII_ACC_PHY_ADDRESS | (index as u32 & 0x1F) << 6 | direction | MII_ACC_BUSY)?;
        self.wait(host, MII_ACC, MII_ACC_BUSY, 0, "the PHY")
    }

    fn phy_read(&self, host: &mut Host, index: u8) -> Result<u16, Lan7800Error> {
        self.wait(host, MII_ACC, MII_ACC_BUSY, 0, "the PHY")?;
        self.phy_access(host, index, false)?;
        Ok(self.read(host, MII_DATA)? as u16)
    }

    fn phy_write(&self, host: &mut Host, index: u8, value: u16) -> Result<(), Lan7800Error> {
        self.wait(host, MII_ACC, MII_ACC_BUSY, 0, "the PHY")?;
        self.write(host, MII_DATA, value as u32)?;
        self.phy_access(host, index, true)
    }
}
