// usb.rs
//! The Synopsys DesignWare USB 2.0 OTG controller (DWC2) of the BCM2835 family, as a USB host:
//! powering it on, resetting it into host mode, bringing up its one root port, and control
//! transfers on endpoint 0, polled, through DMA on one of its host channels: the `Bus` that
//! enumeration (`rustypi_core::usb::tree`) runs on.
//!
//! On the Pi 3 B+ the root port leads to the LAN7515: a hub, a second hub behind it, and the
//! LAN7800 Ethernet controller. Register names follow Circle's `dwhci.h`
//! (<https://github.com/rsta2/circle>).

use core::arch::asm;
use core::fmt;
use core::time::Duration;
use rustypi_core::mailbox::BusAddress;
use rustypi_core::usb::tree::{Bus, Target, Translator};
use rustypi_core::usb::{Direction, SetupPacket, Speed};
use crate::board::PERIPHERAL_BASE;
use crate::drivers::mailbox::tags::{DeviceId, PowerState, SetPowerState};
use crate::drivers::mailbox::{query, Mailbox, MailboxError};
use crate::drivers::mmio::{read, write};
use crate::drivers::timer;
use crate::sched;

const USB_BASE: usize = PERIPHERAL_BASE + 0x98_0000;

// Core registers.
const AHB_CFG: usize = USB_BASE + 0x008;
const USB_CFG: usize = USB_BASE + 0x00C;
const RESET: usize = USB_BASE + 0x010;
const INT_STATUS: usize = USB_BASE + 0x014;
const RX_FIFO_SIZE: usize = USB_BASE + 0x024;
const NP_TX_FIFO_SIZE: usize = USB_BASE + 0x028;
const VENDOR_ID: usize = USB_BASE + 0x040;
const HW_CFG2: usize = USB_BASE + 0x048;
const HOST_PERIODIC_TX_FIFO_SIZE: usize = USB_BASE + 0x100;
const POWER: usize = USB_BASE + 0xE00;

// Host registers.
const HOST_CFG: usize = USB_BASE + 0x400;
/// The (micro)frame number: at high speed, the low 3 bits are the microframe.
const HOST_FRAME_NUMBER: usize = USB_BASE + 0x408;
const HOST_PORT: usize = USB_BASE + 0x440;

/// Host channel `n`'s registers, 0x20 apart.
const fn channel(n: usize, register: usize) -> usize {
    USB_BASE + 0x500 + n * 0x20 + register
}
const CHAN_CHARACTER: usize = 0x00;
const CHAN_SPLIT: usize = 0x04;
const CHAN_INT: usize = 0x08;
const CHAN_INT_MASK: usize = 0x0C;
const CHAN_XFER_SIZE: usize = 0x10;
const CHAN_DMA: usize = 0x14;

/// The vendor ID register's top half: "OT", for OTG.
const VENDOR_OT: u32 = 0x4F54;

const AHB_CFG_GLOBAL_INT: u32 = 1 << 0;
const AHB_CFG_DMA_ENABLE: u32 = 1 << 5;
/// BCM2835 specific: wait for AXI writes to complete before reporting a DMA done.
const AHB_CFG_WAIT_AXI_WRITES: u32 = 1 << 4;
const AHB_CFG_MAX_AXI_BURST: u32 = 3 << 1;

const USB_CFG_PHYIF: u32 = 1 << 3;
const USB_CFG_ULPI_UTMI_SEL: u32 = 1 << 4;
const USB_CFG_ULPI_FSLS: u32 = 1 << 17;
const USB_CFG_ULPI_CLK_SUS_M: u32 = 1 << 19;
const USB_CFG_ULPI_EXT_VBUS_DRV: u32 = 1 << 20;
const USB_CFG_TERM_SEL_DL_PULSE: u32 = 1 << 22;
const USB_CFG_FORCE_HOST_MODE: u32 = 1 << 29;

const RESET_SOFT: u32 = 1 << 0;
const RESET_RX_FIFO_FLUSH: u32 = 1 << 4;
const RESET_TX_FIFO_FLUSH: u32 = 1 << 5;
/// In the TX FIFO number field: all of them.
const RESET_TX_FIFO_ALL: u32 = 0x10 << 6;
const RESET_AHB_IDLE: u32 = 1 << 31;

const INT_STATUS_HOST_MODE: u32 = 1 << 0;

const PORT_CONNECT: u32 = 1 << 0;
const PORT_ENABLE: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 8;
const PORT_POWER: u32 = 1 << 12;
/// Bits cleared by writing 1 (or, for ENABLE, disabling the port): masked out of every write
/// that means to change something else.
const PORT_WRITE_CLEARS: u32 = 1 << 1 | 1 << 2 | 1 << 3 | 1 << 5;

const CHAN_SPLIT_ENABLE: u32 = 1 << 31;
const CHAN_SPLIT_COMPLETE: u32 = 1 << 16;
/// Transaction position: all of it, in one split (never more than a packet for control).
const CHAN_SPLIT_ALL: u32 = 3;

const CHAR_EP_IN: u32 = 1 << 15;
const CHAR_LOW_SPEED: u32 = 1 << 17;
const CHAR_MULTI_COUNT_1: u32 = 1 << 20;
const CHAR_ODD_FRAME: u32 = 1 << 29;
const CHAR_DISABLE: u32 = 1 << 30;
const CHAR_ENABLE: u32 = 1 << 31;

const INT_XFER_COMPLETE: u32 = 1 << 0;
const INT_HALTED: u32 = 1 << 1;
const INT_STALL: u32 = 1 << 3;
const INT_NAK: u32 = 1 << 4;
const INT_ACK: u32 = 1 << 5;
const INT_ERRORS: u32 = 1 << 2 | 1 << 3 | 1 << 7 | 1 << 8 | 1 << 9 | 1 << 10;

/// Data PIDs, in the transfer size register.
#[derive(Clone, Copy)]
enum Pid {
    Data0 = 0,
    Data1 = 2,
    Setup = 3,
}

impl Pid {
    /// The next data packet's PID. (Only data packets toggle.)
    fn toggled(self) -> Pid {
        match self {
            Pid::Data0 => Pid::Data1,
            Pid::Data1 | Pid::Setup => Pid::Data0,
        }
    }
}

/// An endpoint's data toggle: which of DATA0 and DATA1 its next packet carries. Each
/// interrupt or bulk endpoint keeps its own, starting at DATA0 once the device is configured.
#[derive(Clone, Copy)]
pub struct Toggle(Pid);

impl Toggle {
    pub const fn new() -> Self {
        Toggle(Pid::Data0)
    }
}

impl Default for Toggle {
    fn default() -> Self {
        Self::new()
    }
}

/// An interrupt IN endpoint, as `Host::interrupt_in` polls it.
#[derive(Clone, Copy)]
pub struct InterruptIn {
    pub target: Target,
    pub endpoint: u8,
    pub max_packet: u16,
}

/// A bulk endpoint of a device, one way.
#[derive(Clone, Copy)]
pub struct BulkEndpoint {
    pub target: Target,
    pub endpoint: u8,
    pub max_packet: u16,
}

/// FIFO sizes, in 32-bit words. The controller has 4080 in all.
const RX_FIFO_WORDS: u32 = 1024;
const NP_TX_FIFO_WORDS: u32 = 1024;
const PERIODIC_TX_FIFO_WORDS: u32 = 1024;

/// The channel control transfers use.
const CONTROL_CHANNEL: usize = 0;

/// How long to wait for a device on the root port, after powering it.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(510);
/// USB 2.0 timings: debounce after connecting, reset held, recovery after reset.
const DEBOUNCE: Duration = Duration::from_millis(100);
const RESET_HOLD: Duration = Duration::from_millis(50);
const RESET_RECOVERY: Duration = Duration::from_millis(20);
const TRANSFER_TIMEOUT_US: u64 = 500_000;
/// How long one split packet may take, retries and all.
const SPLIT_TIMEOUT_US: u64 = 1_000_000;
/// Transaction errors a split packet may run into before giving up, and complete splits the
/// translator may answer "not yet" before the packet starts over (Circle's figures).
const MAX_SPLIT_ERRORS: u32 = 3;
const MAX_NYETS: u32 = 3;
/// A high speed microframe.
const MICROFRAME_US: u64 = 125;
/// Longer than any wait for a particular (micro)frame should take: a frame and a bit.
const FRAME_WAIT_US: u64 = 2_000;
const REGISTER_TIMEOUT_US: u64 = 100_000;

const CACHE_LINE: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum UsbError {
    Mailbox(MailboxError),
    /// The firmware didn't power the controller on.
    NoPower,
    /// The registers aren't a DWC2's.
    NotDwc2(u32),
    Timeout(&'static str),
    /// No device on the root port.
    NoDevice,
    /// A transfer stage ended with these channel interrupt bits.
    Transfer { stage: &'static str, interrupts: u32 },
    Stalled(&'static str),
    TooLong,
}

impl From<MailboxError> for UsbError {
    fn from(error: MailboxError) -> Self {
        UsbError::Mailbox(error)
    }
}

impl fmt::Display for UsbError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UsbError::Mailbox(error) => write!(f, "mailbox: {error}"),
            UsbError::NoPower => write!(f, "the firmware didn't power the controller on"),
            UsbError::NotDwc2(id) => write!(f, "not a DWC2 controller (vendor id {id:#x})"),
            UsbError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            UsbError::NoDevice => write!(f, "nothing on the root port"),
            UsbError::Transfer { stage, interrupts } => write!(f, "{stage} stage failed (channel interrupts {interrupts:#x})"),
            UsbError::Stalled(stage) => write!(f, "device stalled the {stage} stage"),
            UsbError::TooLong => write!(f, "transfer too long for the buffer"),
        }
    }
}

fn wait_for(what: &'static str, timeout_us: u64, done: impl Fn() -> bool) -> Result<(), UsbError> {
    let start = timer::now_us();
    while !done() {
        if timer::now_us() - start > timeout_us {
            return Err(UsbError::Timeout(what));
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Memory the controller reads and writes by DMA. Its cache lines are written back before the
/// controller reads them and dropped once it has written them, since the controller doesn't see
/// the ARM caches. A whole number of lines, so nothing else shares them.
#[repr(C, align(64))]
struct DmaBuffer<const N: usize>([u8; N]);

impl<const N: usize> DmaBuffer<N> {
    const WHOLE_LINES: () = assert!(N % CACHE_LINE == 0);

    const fn new() -> Self {
        let () = Self::WHOLE_LINES;
        DmaBuffer([0; N])
    }

    fn bus_address(&self) -> u32 {
        BusAddress::from_arm(self.0.as_ptr() as usize).0
    }

    /// Writes the buffer back to memory, and drops it from the cache, so neither what the CPU
    /// wrote nor stale lines get in the controller's way.
    fn clean_and_invalidate(&self) {
        let start = self.0.as_ptr() as usize;
        for line in (start..start + self.0.len()).step_by(CACHE_LINE) {
            unsafe { asm!("dc civac, {}", in(reg) line, options(nostack, preserves_flags)) };
        }
        unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
    }
}

/// What the probe found.
#[derive(Clone, Copy, Debug)]
pub struct Controller {
    /// The vendor ID register's low half: the core's version, like 0x280A for 2.80a.
    pub version: u16,
    pub channels: u32,
}

/// The root port once a device on it is reset and enabled.
#[derive(Clone, Copy, Debug)]
pub struct RootPort {
    pub speed: Speed,
}

/// The controller, powered on and in host mode, with its root port up. Only one exists: it
/// owns the controller's registers and its DMA buffer.
pub struct Host {
    pub controller: Controller,
    pub port: RootPort,
    /// For control and interrupt transfers.
    buffer: DmaBuffer<512>,
    /// For bulk transfers: big enough for a burst of received Ethernet frames, and for one
    /// frame to send.
    bulk_in: DmaBuffer<4096>,
    bulk_out: DmaBuffer<2048>,
}

impl Host {
    /// Powers the controller on, resets it into host mode and brings up the root port. Takes a
    /// second or so, sleeping in between.
    pub fn start() -> Result<Host, UsbError> {
        let power = query::<SetPowerState>(&Mailbox, PowerState::on(DeviceId::USB_HCD))?;
        if !power.is_on() {
            return Err(UsbError::NoPower);
        }
        let id = read(VENDOR_ID);
        if id >> 16 != VENDOR_OT {
            return Err(UsbError::NotDwc2(id));
        }
        let hw_cfg2 = read(HW_CFG2);
        let controller = Controller { version: id as u16, channels: (hw_cfg2 >> 14 & 0xF) + 1 };

        // Polled: no interrupts from it.
        write(AHB_CFG, read(AHB_CFG) & !AHB_CFG_GLOBAL_INT);
        init_core(hw_cfg2)?;
        init_host(hw_cfg2)?;
        let port = enable_root_port()?;
        Ok(Host { controller, port, buffer: DmaBuffer::new(), bulk_in: DmaBuffer::new(), bulk_out: DmaBuffer::new() })
    }

    /// Waits `ms` milliseconds, letting other tasks run.
    fn sleep_ms(ms: u32) {
        sched::sleep(Duration::from_millis(ms as u64));
    }
}

impl Bus for Host {
    type Error = UsbError;

    /// The setup stage, a data stage in `setup`'s direction if it has a length, and the status
    /// stage, through the DMA buffer.
    fn control(&mut self, target: Target, setup: SetupPacket, data: &mut [u8]) -> Result<usize, UsbError> {
        let length = setup.length as usize;
        if length > self.buffer.0.len() || length > data.len() {
            return Err(UsbError::TooLong);
        }

        self.buffer.0[..8].copy_from_slice(&setup.to_bytes());
        self.transfer(target, Direction::Out, Pid::Setup, 8, "setup")?;

        let direction = setup.direction();
        let mut received = 0;
        if length > 0 {
            if direction == Direction::Out {
                self.buffer.0[..length].copy_from_slice(&data[..length]);
            }
            received = self.transfer(target, direction, Pid::Data1, length, "data")?;
            if direction == Direction::In {
                data[..received].copy_from_slice(&self.buffer.0[..received]);
            }
        }
        // The status stage goes the other way, and is always DATA1.
        let status_direction = if length > 0 && direction == Direction::In { Direction::Out } else { Direction::In };
        self.transfer(target, status_direction, Pid::Data1, 0, "status")?;
        Ok(if direction == Direction::In { received } else { 0 })
    }

    fn delay_ms(&mut self, ms: u32) {
        Host::sleep_ms(ms);
    }
}

impl Host {

    /// One stage: `length` bytes from (OUT) or into (IN) the buffer. Returns how many moved.
    fn transfer(&mut self, target: Target, direction: Direction, pid: Pid, length: usize, stage: &'static str) -> Result<usize, UsbError> {
        let n = CONTROL_CHANNEL;
        // Halt it if a failed transfer left it going.
        if read(channel(n, CHAN_CHARACTER)) & CHAR_ENABLE != 0 {
            write(channel(n, CHAN_CHARACTER), read(channel(n, CHAN_CHARACTER)) | CHAR_DISABLE);
            wait_for("a channel to halt", REGISTER_TIMEOUT_US, || read(channel(n, CHAN_INT)) & INT_HALTED != 0)?;
        }
        match target.translator {
            None => self.direct_transfer(target, direction, pid, length, stage),
            Some(translator) => self.split_transfer(target, translator, direction, pid, length, stage),
        }
    }

    /// A stage straight to a device at the bus's own speed: all its packets in one go.
    fn direct_transfer(&mut self, target: Target, direction: Direction, pid: Pid, length: usize, stage: &'static str) -> Result<usize, UsbError> {
        let packets = length.div_ceil(target.max_packet as usize).max(1) as u32;
        loop {
            let (interrupts, remaining) = self.run_channel(Transaction::control(target, direction, pid, 0, length, packets))?;
            if interrupts & INT_STALL != 0 {
                return Err(UsbError::Stalled(stage));
            }
            // The device wasn't ready: ask again.
            if interrupts & INT_NAK != 0 && interrupts & INT_XFER_COMPLETE == 0 {
                sched::sleep(Duration::from_millis(1));
                continue;
            }
            if interrupts & INT_ERRORS != 0 || interrupts & INT_XFER_COMPLETE == 0 {
                return Err(UsbError::Transfer { stage, interrupts });
            }
            return Ok(length - remaining);
        }
    }

    /// A stage to a low or full speed device through a high speed hub's transaction
    /// translator: a packet at a time, each a start split the hub takes on, then complete
    /// splits until the hub has the device's answer. Like Circle's non-periodic split
    /// scheduling.
    fn split_transfer(
        &mut self,
        target: Target,
        translator: Translator,
        direction: Direction,
        pid: Pid,
        length: usize,
        stage: &'static str,
    ) -> Result<usize, UsbError> {
        let started = timer::now_us();
        let mut done = 0;
        let mut pid = pid;
        loop {
            let size = (length - done).min(target.max_packet as usize);
            let moved = self.split_packet(target, translator, direction, pid, done, size, stage, started)?;
            done += moved;
            pid = pid.toggled();
            // A short packet ends the stage early.
            if moved < size || done >= length {
                return Ok(done);
            }
        }
    }

    /// One packet of a split stage, `size` bytes at `offset` in the buffer. Returns how many
    /// bytes moved.
    #[allow(clippy::too_many_arguments)]
    fn split_packet(
        &mut self,
        target: Target,
        translator: Translator,
        direction: Direction,
        pid: Pid,
        offset: usize,
        size: usize,
        stage: &'static str,
        started: u64,
    ) -> Result<usize, UsbError> {
        let split = CHAN_SPLIT_ENABLE
            | CHAN_SPLIT_ALL << 14
            | (translator.hub as u32 & 0x7F) << 7
            | translator.port as u32 & 0x7F;
        let mut errors = 0;
        'start: loop {
            if timer::now_us() - started > SPLIT_TIMEOUT_US {
                return Err(UsbError::Timeout("a split transaction"));
            }
            let (interrupts, _) = self.run_channel(Transaction { split, ..Transaction::control(target, direction, pid, offset, size, 1) })?;
            if interrupts & INT_STALL != 0 {
                return Err(UsbError::Stalled(stage));
            }
            if interrupts & INT_ACK == 0 {
                // The translator is busy (NAK), or the start split was lost: try again.
                if interrupts & INT_ERRORS != 0 {
                    errors += 1;
                    if errors > MAX_SPLIT_ERRORS {
                        return Err(UsbError::Transfer { stage, interrupts });
                    }
                }
                timer::delay_us(MICROFRAME_US);
                continue 'start;
            }

            let mut nyets = 0;
            loop {
                let complete = Transaction { split: split | CHAN_SPLIT_COMPLETE, ..Transaction::control(target, direction, pid, offset, size, 1) };
                let (interrupts, remaining) = self.run_channel(complete)?;
                if interrupts & INT_STALL != 0 {
                    return Err(UsbError::Stalled(stage));
                }
                if interrupts & INT_XFER_COMPLETE != 0 {
                    return Ok(size - remaining);
                }
                if interrupts & INT_NAK != 0 {
                    // The device wasn't ready: start the packet over, a little later.
                    timer::delay_us(5 * MICROFRAME_US);
                    continue 'start;
                }
                if interrupts & INT_ERRORS != 0 {
                    errors += 1;
                    if errors > MAX_SPLIT_ERRORS {
                        return Err(UsbError::Transfer { stage, interrupts });
                    }
                    continue 'start;
                }
                // NYET: the translator doesn't have the answer yet.
                nyets += 1;
                if nyets > MAX_NYETS {
                    continue 'start;
                }
                timer::delay_us(5 * MICROFRAME_US);
            }
        }
    }

    /// Polls an interrupt IN endpoint once: `Some` of the bytes it sent into `data`, or `None`
    /// if it had nothing new (it NAKed, as a keyboard does between key changes). `toggle` is
    /// the endpoint's, and moves on when data arrives.
    pub fn interrupt_in(&mut self, endpoint: InterruptIn, toggle: &mut Toggle, data: &mut [u8]) -> Result<Option<usize>, UsbError> {
        let length = (endpoint.max_packet as usize).min(data.len()).min(self.buffer.0.len());
        let base = Transaction {
            target: endpoint.target,
            endpoint: endpoint.endpoint,
            kind: Kind::Interrupt,
            direction: Direction::In,
            pid: toggle.0,
            offset: 0,
            length,
            packets: 1,
            split: 0,
            odd_frame: false,
            buffer: Buffer::Control,
        };
        let received = match endpoint.target.translator {
            None => self.direct_interrupt(base)?,
            Some(translator) => self.split_interrupt(base, translator)?,
        };
        if let Some(count) = received {
            data[..count].copy_from_slice(&self.buffer.0[..count]);
            toggle.0 = toggle.0.toggled();
        }
        Ok(received)
    }

    /// An interrupt transaction straight to the device, in the next frame.
    fn direct_interrupt(&mut self, t: Transaction) -> Result<Option<usize>, UsbError> {
        let now = frame_number();
        wait_for("the next frame", FRAME_WAIT_US, || frame_number() != now)?;
        let (interrupts, remaining) = self.run_channel(Transaction { odd_frame: frame_number() & 1 != 0, ..t })?;
        if interrupts & INT_XFER_COMPLETE != 0 {
            return Ok(Some(t.length - remaining));
        }
        interrupt_outcome(interrupts)
    }

    /// An interrupt transaction through a transaction translator, on the microframe schedule
    /// periodic splits need, like Circle's periodic scheduler: the start split in the next
    /// microframe (never the 6th of a frame), complete splits from two microframes later.
    fn split_interrupt(&mut self, t: Transaction, translator: Translator) -> Result<Option<usize>, UsbError> {
        let split = CHAN_SPLIT_ENABLE
            | CHAN_SPLIT_ALL << 14
            | (translator.hub as u32 & 0x7F) << 7
            | translator.port as u32 & 0x7F;
        for _ in 0..MAX_SPLIT_ERRORS {
            let mut next = (frame_number() + 1) & 7;
            if next == 6 {
                next = 7;
            }
            wait_for_microframe(next)?;
            let (interrupts, _) = self.run_channel(Transaction { split, odd_frame: frame_number() & 1 != 0, ..t })?;
            if interrupts & INT_ACK == 0 {
                // The translator had no room for it: nothing this time.
                return interrupt_outcome(interrupts);
            }

            let mut tries = if next == 5 { 2 } else { 3 };
            next = (next + 2) & 7;
            loop {
                wait_for_microframe(next)?;
                let complete = Transaction { split: split | CHAN_SPLIT_COMPLETE, odd_frame: frame_number() & 1 != 0, ..t };
                let (interrupts, remaining) = self.run_channel(complete)?;
                if interrupts & INT_XFER_COMPLETE != 0 {
                    return Ok(Some(t.length - remaining));
                }
                if interrupts & (INT_NAK | INT_STALL | INT_ERRORS) != 0 {
                    return interrupt_outcome(interrupts);
                }
                // NYET: the answer isn't through the translator yet.
                if tries == 0 {
                    break;
                }
                tries -= 1;
                next = (next + 1) & 7;
            }
            // Out of complete splits: start the transaction over, after a frame.
            timer::delay_us(8 * MICROFRAME_US);
        }
        Err(UsbError::Timeout("a periodic split transaction"))
    }

    /// Sends `data` to a bulk OUT endpoint, in as many packets as it takes. `toggle` is the
    /// endpoint's.
    pub fn bulk_out(&mut self, endpoint: BulkEndpoint, toggle: &mut Toggle, data: &[u8]) -> Result<(), UsbError> {
        if data.len() > self.bulk_out.0.len() {
            return Err(UsbError::TooLong);
        }
        self.bulk_out.0[..data.len()].copy_from_slice(data);
        self.bulk(endpoint, Direction::Out, toggle, data.len()).map(|_| ())
    }

    /// Takes what a bulk IN endpoint has, up to a burst of 4KB (fewer bytes, or none, if it
    /// has less). `toggle` is the endpoint's.
    pub fn bulk_in(&mut self, endpoint: BulkEndpoint, toggle: &mut Toggle) -> Result<&[u8], UsbError> {
        let length = self.bulk_in.0.len();
        let received = self.bulk(endpoint, Direction::In, toggle, length)?;
        Ok(&self.bulk_in.0[..received])
    }

    /// A bulk transfer of up to `length` bytes, from or into the bulk buffer that way. The
    /// controller keeps the data toggle across the packets and says where it ended up; a
    /// transfer halted part way (NAKed) carries on from there. Returns the bytes moved.
    fn bulk(&mut self, endpoint: BulkEndpoint, direction: Direction, toggle: &mut Toggle, length: usize) -> Result<usize, UsbError> {
        let max_packet = endpoint.max_packet.max(1) as usize;
        let mut done = 0;
        let started = timer::now_us();
        loop {
            let left = length - done;
            let packets = left.div_ceil(max_packet).max(1) as u32;
            let t = Transaction {
                target: endpoint.target,
                endpoint: endpoint.endpoint,
                kind: Kind::Bulk,
                direction,
                pid: toggle.0,
                offset: done,
                length: left,
                packets,
                split: 0,
                odd_frame: false,
                buffer: if direction == Direction::In { Buffer::BulkIn } else { Buffer::BulkOut },
            };
            let (interrupts, remaining) = self.run_channel(t)?;
            done += left - remaining;
            toggle.0 = channel_pid();
            if interrupts & INT_STALL != 0 {
                return Err(UsbError::Stalled("bulk"));
            }
            if interrupts & INT_XFER_COMPLETE != 0 {
                return Ok(done);
            }
            if interrupts & INT_NAK == 0 || interrupts & INT_ERRORS != 0 {
                return Err(UsbError::Transfer { stage: "bulk", interrupts });
            }
            if timer::now_us() - started > TRANSFER_TIMEOUT_US {
                return Err(UsbError::Timeout("a bulk transfer"));
            }
            sched::sleep(Duration::from_millis(1));
        }
    }

    /// Runs one transaction (or, direct, a whole control stage) on the channel and waits for
    /// it to halt. Returns the channel's interrupt bits and how many of its bytes weren't moved.
    fn run_channel(&mut self, t: Transaction) -> Result<(u32, usize), UsbError> {
        let n = CONTROL_CHANNEL;
        let (bus_address, clean): (u32, &dyn Fn()) = match t.buffer {
            Buffer::Control => (self.buffer.bus_address(), &|| self.buffer.clean_and_invalidate()),
            Buffer::BulkIn => (self.bulk_in.bus_address(), &|| self.bulk_in.clean_and_invalidate()),
            Buffer::BulkOut => (self.bulk_out.bus_address(), &|| self.bulk_out.clean_and_invalidate()),
        };
        clean();
        write(channel(n, CHAN_INT), u32::MAX);
        write(channel(n, CHAN_INT_MASK), 0);
        write(channel(n, CHAN_SPLIT), t.split);
        write(channel(n, CHAN_XFER_SIZE), t.length as u32 | t.packets << 19 | (t.pid as u32) << 29);
        write(channel(n, CHAN_DMA), bus_address + t.offset as u32);
        let mut character = t.target.max_packet as u32 & 0x7FF
            | (t.endpoint as u32 & 0xF) << 11
            | (t.kind as u32) << 18
            | CHAR_MULTI_COUNT_1
            | (t.target.address as u32 & 0x7F) << 22;
        if t.direction == Direction::In {
            character |= CHAR_EP_IN;
        }
        if t.target.speed == Speed::Low {
            character |= CHAR_LOW_SPEED;
        }
        if t.odd_frame {
            character |= CHAR_ODD_FRAME;
        }
        write(channel(n, CHAN_CHARACTER), character | CHAR_ENABLE);

        wait_for("a transfer", TRANSFER_TIMEOUT_US, || read(channel(n, CHAN_INT)) & INT_HALTED != 0)?;
        let interrupts = read(channel(n, CHAN_INT));
        // DMA may have written the buffer: drop anything the CPU cached of it meanwhile.
        clean();
        let remaining = (read(channel(n, CHAN_XFER_SIZE)) & 0x7FFFF) as usize;
        Ok((interrupts, remaining.min(t.length)))
    }
}

/// Endpoint types, as the channel characteristics register has them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Control = 0,
    Bulk = 2,
    Interrupt = 3,
}

/// Which DMA buffer a transaction's data is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Buffer {
    Control,
    BulkIn,
    BulkOut,
}

/// One transaction for the channel to run.
#[derive(Clone, Copy)]
struct Transaction {
    target: Target,
    endpoint: u8,
    kind: Kind,
    direction: Direction,
    pid: Pid,
    /// Where in the DMA buffer its data is, and how much.
    offset: usize,
    length: usize,
    packets: u32,
    /// The split register: 0 for none.
    split: u32,
    /// For periodic transactions: run in an odd (micro)frame, rather than an even one.
    odd_frame: bool,
    buffer: Buffer,
}

impl Transaction {
    /// A control stage's packets on endpoint 0.
    fn control(target: Target, direction: Direction, pid: Pid, offset: usize, length: usize, packets: u32) -> Self {
        Transaction {
            target,
            endpoint: 0,
            kind: Kind::Control,
            direction,
            pid,
            offset,
            length,
            packets,
            split: 0,
            odd_frame: false,
            buffer: Buffer::Control,
        }
    }
}

/// What an interrupt transaction that didn't bring data means: nothing new (NAK), or an error.
fn interrupt_outcome(interrupts: u32) -> Result<Option<usize>, UsbError> {
    if interrupts & INT_STALL != 0 {
        Err(UsbError::Stalled("interrupt"))
    } else if interrupts & INT_NAK != 0 {
        Ok(None)
    } else {
        Err(UsbError::Transfer { stage: "interrupt", interrupts })
    }
}

/// The data PID the channel's next packet would carry, after a transfer: where the endpoint's
/// data toggle is now.
fn channel_pid() -> Pid {
    match read(channel(CONTROL_CHANNEL, CHAN_XFER_SIZE)) >> 29 & 3 {
        2 => Pid::Data1,
        _ => Pid::Data0,
    }
}

/// The current (micro)frame number.
fn frame_number() -> u32 {
    read(HOST_FRAME_NUMBER) & 0xFFFF
}

/// Waits until the bus is in microframe `microframe` (0 to 7) of a frame.
fn wait_for_microframe(microframe: u32) -> Result<(), UsbError> {
    wait_for("a microframe", FRAME_WAIT_US, || frame_number() & 7 == microframe)
}

/// Resets the core and sets up its PHY and DMA, like Circle's `InitCore`.
fn init_core(hw_cfg2: u32) -> Result<(), UsbError> {
    let mut usb_cfg = read(USB_CFG);
    usb_cfg &= !(USB_CFG_ULPI_EXT_VBUS_DRV | USB_CFG_TERM_SEL_DL_PULSE);
    write(USB_CFG, usb_cfg);

    wait_for("the AHB to go idle", REGISTER_TIMEOUT_US, || read(RESET) & RESET_AHB_IDLE != 0)?;
    write(RESET, RESET_SOFT);
    wait_for("the core to reset", REGISTER_TIMEOUT_US, || read(RESET) & RESET_SOFT == 0)?;
    sched::sleep(Duration::from_millis(100));

    // UTMI+ PHY, 8 bits wide.
    let mut usb_cfg = read(USB_CFG) & !(USB_CFG_ULPI_UTMI_SEL | USB_CFG_PHYIF);
    let ulpi_hs = hw_cfg2 >> 6 & 3 == 2;
    let dedicated_fs = hw_cfg2 >> 8 & 3 == 1;
    if ulpi_hs && dedicated_fs {
        usb_cfg |= USB_CFG_ULPI_FSLS | USB_CFG_ULPI_CLK_SUS_M;
    } else {
        usb_cfg &= !(USB_CFG_ULPI_FSLS | USB_CFG_ULPI_CLK_SUS_M);
    }
    // Host, whatever the OTG ID pin says. The mode takes up to 25ms to switch.
    write(USB_CFG, usb_cfg | USB_CFG_FORCE_HOST_MODE);
    sched::sleep(Duration::from_millis(50));
    wait_for("host mode", REGISTER_TIMEOUT_US, || read(INT_STATUS) & INT_STATUS_HOST_MODE != 0)?;

    write(AHB_CFG, (read(AHB_CFG) & !AHB_CFG_MAX_AXI_BURST) | AHB_CFG_DMA_ENABLE | AHB_CFG_WAIT_AXI_WRITES);
    Ok(())
}

/// Sets up the host side: PHY clock, FIFOs, and power to the root port.
fn init_host(hw_cfg2: u32) -> Result<(), UsbError> {
    // Restart the PHY clock.
    write(POWER, 0);
    let ulpi_hs = hw_cfg2 >> 6 & 3 == 2;
    let dedicated_fs = hw_cfg2 >> 8 & 3 == 1;
    // FS/LS PHY clock: 48MHz for a dedicated full speed PHY behind ULPI, else 30/60MHz.
    let clock = if ulpi_hs && dedicated_fs { 1 } else { 0 };
    write(HOST_CFG, (read(HOST_CFG) & !3) | clock);

    write(RX_FIFO_SIZE, RX_FIFO_WORDS);
    write(NP_TX_FIFO_SIZE, RX_FIFO_WORDS | NP_TX_FIFO_WORDS << 16);
    write(HOST_PERIODIC_TX_FIFO_SIZE, (RX_FIFO_WORDS + NP_TX_FIFO_WORDS) | PERIODIC_TX_FIFO_WORDS << 16);

    write(RESET, RESET_TX_FIFO_FLUSH | RESET_TX_FIFO_ALL);
    wait_for("the TX FIFOs to flush", REGISTER_TIMEOUT_US, || read(RESET) & RESET_TX_FIFO_FLUSH == 0)?;
    write(RESET, RESET_RX_FIFO_FLUSH);
    wait_for("the RX FIFO to flush", REGISTER_TIMEOUT_US, || read(RESET) & RESET_RX_FIFO_FLUSH == 0)?;

    let port = read(HOST_PORT) & !PORT_WRITE_CLEARS;
    if port & PORT_POWER == 0 {
        write(HOST_PORT, port | PORT_POWER);
    }
    Ok(())
}

/// Waits for a device on the root port, resets it, and reports its speed.
fn enable_root_port() -> Result<RootPort, UsbError> {
    wait_for("a device on the root port", CONNECT_TIMEOUT.as_micros() as u64, || read(HOST_PORT) & PORT_CONNECT != 0)
        .map_err(|_| UsbError::NoDevice)?;
    sched::sleep(DEBOUNCE);

    let port = read(HOST_PORT) & !PORT_WRITE_CLEARS;
    write(HOST_PORT, port | PORT_RESET);
    sched::sleep(RESET_HOLD);
    write(HOST_PORT, read(HOST_PORT) & !PORT_WRITE_CLEARS & !PORT_RESET);
    sched::sleep(RESET_RECOVERY);

    let port = read(HOST_PORT);
    if port & PORT_ENABLE == 0 {
        return Err(UsbError::Timeout("the root port to enable"));
    }
    let speed = match port >> 17 & 3 {
        0 => Speed::High,
        1 => Speed::Full,
        _ => Speed::Low,
    };
    Ok(RootPort { speed })
}
