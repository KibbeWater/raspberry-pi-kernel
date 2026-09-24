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
const CHAR_EP_CONTROL: u32 = 0 << 18;
const CHAR_MULTI_COUNT_1: u32 = 1 << 20;
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
struct DmaBuffer([u8; 512]);

impl DmaBuffer {
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
    buffer: DmaBuffer,
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
        Ok(Host { controller, port, buffer: DmaBuffer([0; 512]) })
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
            let (interrupts, remaining) = self.run_channel(target, direction, pid, 0, length, packets, 0)?;
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
            return Ok(length - remaining.min(length));
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
            let (interrupts, _) = self.run_channel(target, direction, pid, offset, size, 1, split)?;
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
                let (interrupts, remaining) = self.run_channel(target, direction, pid, offset, size, 1, split | CHAN_SPLIT_COMPLETE)?;
                if interrupts & INT_STALL != 0 {
                    return Err(UsbError::Stalled(stage));
                }
                if interrupts & INT_XFER_COMPLETE != 0 {
                    return Ok(size - remaining.min(size));
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

    /// Runs one transaction (or, direct, a whole stage) on the control channel and waits for
    /// it to halt. `split` goes into the split register. Returns the channel's interrupt bits
    /// and how many of `length` bytes weren't moved.
    #[allow(clippy::too_many_arguments)]
    fn run_channel(
        &mut self,
        target: Target,
        direction: Direction,
        pid: Pid,
        offset: usize,
        length: usize,
        packets: u32,
        split: u32,
    ) -> Result<(u32, usize), UsbError> {
        let n = CONTROL_CHANNEL;
        self.buffer.clean_and_invalidate();
        write(channel(n, CHAN_INT), u32::MAX);
        write(channel(n, CHAN_INT_MASK), 0);
        write(channel(n, CHAN_SPLIT), split);
        write(channel(n, CHAN_XFER_SIZE), length as u32 | packets << 19 | (pid as u32) << 29);
        write(channel(n, CHAN_DMA), self.buffer.bus_address() + offset as u32);
        let mut character = target.max_packet as u32 & 0x7FF
            | CHAR_EP_CONTROL
            | CHAR_MULTI_COUNT_1
            | (target.address as u32 & 0x7F) << 22;
        if direction == Direction::In {
            character |= CHAR_EP_IN;
        }
        if target.speed == Speed::Low {
            character |= CHAR_LOW_SPEED;
        }
        write(channel(n, CHAN_CHARACTER), character | CHAR_ENABLE);

        wait_for("a transfer", TRANSFER_TIMEOUT_US, || read(channel(n, CHAN_INT)) & INT_HALTED != 0)?;
        let interrupts = read(channel(n, CHAN_INT));
        // DMA may have written the buffer: drop anything the CPU cached of it meanwhile.
        self.buffer.clean_and_invalidate();
        let remaining = (read(channel(n, CHAN_XFER_SIZE)) & 0x7FFFF) as usize;
        Ok((interrupts, remaining))
    }
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
