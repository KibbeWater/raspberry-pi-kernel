// sdcard.rs
//! SD card through the Arasan EMMC controller (an SDHCI host), read-only, 1-bit bus at
//! 25MHz, polled. Follows the SD Physical Layer Simplified Specification's initialisation:
//! reset, CMD0, CMD8, ACMD41 until ready, CMD2, CMD3, CMD7.
//!
//! On the Pi 3 the card slot's pins (GPIO 48-53) are wired to the firmware's SDHOST
//! controller at boot; they are switched over to the EMMC controller here.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
use rustypi_core::block::{Block, BlockDevice, Lba, BLOCK_SIZE};
use rustypi_core::sd;
use crate::board::PERIPHERAL_BASE;
use crate::drivers::gpio::{set_pin_mode, set_pin_pull, Pin, PinMode, PullMode};
use crate::drivers::mailbox::tags::{ClockId, GetClockRate};
use crate::drivers::mailbox::{query, Mailbox, MailboxError};
use crate::drivers::timer;

const EMMC_BASE: usize = PERIPHERAL_BASE + 0x30_0000;
const BLKSIZECNT: usize = EMMC_BASE + 0x04;
const ARG1: usize = EMMC_BASE + 0x08;
const CMDTM: usize = EMMC_BASE + 0x0C;
const RESP0: usize = EMMC_BASE + 0x10;
const DATA: usize = EMMC_BASE + 0x20;
const STATUS: usize = EMMC_BASE + 0x24;
const CONTROL0: usize = EMMC_BASE + 0x28;
const CONTROL1: usize = EMMC_BASE + 0x2C;
const INTERRUPT: usize = EMMC_BASE + 0x30;
const IRPT_MASK: usize = EMMC_BASE + 0x34;
const IRPT_EN: usize = EMMC_BASE + 0x38;
const SLOTISR_VER: usize = EMMC_BASE + 0xFC;

// STATUS
const CMD_INHIBIT: u32 = 1 << 0;
const DAT_INHIBIT: u32 = 1 << 1;

// CONTROL1
const CLK_INTLEN: u32 = 1 << 0;
const CLK_STABLE: u32 = 1 << 1;
const CLK_EN: u32 = 1 << 2;
const CLK_DIVIDER_MASK: u32 = 0xFFC0;
/// Longest data timeout: TMCLK * 2^27.
const DATA_TIMEOUT_MAX: u32 = 0xE << 16;
const SRST_HC: u32 = 1 << 24;
const SRST_CMD: u32 = 1 << 25;

// INTERRUPT
const INT_CMD_DONE: u32 = 1 << 0;
const INT_DATA_DONE: u32 = 1 << 1;
const INT_READ_RDY: u32 = 1 << 5;
const INT_ERROR: u32 = 1 << 15;
const INT_ALL: u32 = 0xFFFF_FFFF;

// CMDTM
const CMD_RESPONSE_136: u32 = 1 << 16;
const CMD_RESPONSE_48: u32 = 2 << 16;
const CMD_RESPONSE_48_BUSY: u32 = 3 << 16;
const CMD_CRC_CHECK: u32 = 1 << 19;
const CMD_INDEX_CHECK: u32 = 1 << 20;
const CMD_DATA: u32 = 1 << 21;
const CMD_READ: u32 = 1 << 4;

const IDENTIFICATION_HZ: u32 = 400_000;
const TRANSFER_HZ: u32 = 25_000_000;

/// CMD8 argument: 2.7-3.6V, and a check pattern the card echoes.
const IF_COND_ARG: u32 = 0x1AA;
/// ACMD41: the voltage window 3.2-3.4V, plus "host supports high capacity".
const OCR_VOLTAGE: u32 = 0x00FF_8000;
const OCR_HCS: u32 = 1 << 30;
const OCR_READY: u32 = 1 << 31;

/// Microsecond timeouts.
const RESET_TIMEOUT: u64 = 100_000;
const COMMAND_TIMEOUT: u64 = 100_000;
const DATA_TIMEOUT: u64 = 500_000;
const POWER_UP_TIMEOUT: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseKind {
    None,
    /// 48 bits with CRC and index (R1, R6, R7).
    Short,
    /// 48 bits, then the card holds DAT0 low while busy (R1b).
    ShortBusy,
    /// 48 bits without CRC or index (R3, the OCR).
    ShortNoCheck,
    /// 136 bits (R2: CID or CSD).
    Long,
}

/// An SD command with the response it produces, so each is always sent with the right
/// response handling.
#[derive(Clone, Copy, Debug)]
struct Command {
    index: u8,
    response: ResponseKind,
    reads_data: bool,
}

impl Command {
    const fn new(index: u8, response: ResponseKind) -> Self {
        Command { index, response, reads_data: false }
    }

    fn encode(self) -> u32 {
        let response = match self.response {
            ResponseKind::None => 0,
            ResponseKind::Short => CMD_RESPONSE_48 | CMD_CRC_CHECK | CMD_INDEX_CHECK,
            ResponseKind::ShortBusy => CMD_RESPONSE_48_BUSY | CMD_CRC_CHECK | CMD_INDEX_CHECK,
            ResponseKind::ShortNoCheck => CMD_RESPONSE_48,
            ResponseKind::Long => CMD_RESPONSE_136 | CMD_CRC_CHECK,
        };
        let data = if self.reads_data { CMD_DATA | CMD_READ } else { 0 };
        (self.index as u32) << 24 | response | data
    }
}

const GO_IDLE_STATE: Command = Command::new(0, ResponseKind::None);
const ALL_SEND_CID: Command = Command::new(2, ResponseKind::Long);
const SEND_RELATIVE_ADDR: Command = Command::new(3, ResponseKind::Short);
const SELECT_CARD: Command = Command::new(7, ResponseKind::ShortBusy);
const SEND_IF_COND: Command = Command::new(8, ResponseKind::Short);
const SEND_CSD: Command = Command::new(9, ResponseKind::Long);
const SET_BLOCKLEN: Command = Command::new(16, ResponseKind::Short);
const READ_SINGLE_BLOCK: Command = Command { index: 17, response: ResponseKind::Short, reads_data: true };
const APP_CMD: Command = Command::new(55, ResponseKind::Short);
/// Application command: must follow APP_CMD.
const SD_SEND_OP_COND: Command = Command::new(41, ResponseKind::ShortNoCheck);

#[derive(Debug)]
pub enum SdError {
    Timeout(&'static str),
    /// The controller flagged an error; `interrupt` holds its error bits.
    Command { index: u8, interrupt: u32 },
    /// The card answered CMD8 with the wrong check pattern.
    BadIfCond(u32),
    Clock(MailboxError),
    /// The firmware reports the EMMC clock as stopped.
    NoClock,
    /// A byte-addressed (standard capacity) card can't reach this block.
    OutOfRange(Lba),
}

impl fmt::Display for SdError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SdError::Timeout(what) => write!(f, "timed out waiting for {}", what),
            SdError::Command { index, interrupt } => {
                write!(f, "CMD{} failed (interrupt {:#x})", index, interrupt)
            }
            SdError::BadIfCond(response) => write!(f, "bad CMD8 response {:#x}", response),
            SdError::Clock(error) => write!(f, "clock: {}", error),
            SdError::NoClock => write!(f, "EMMC clock is off"),
            SdError::OutOfRange(lba) => write!(f, "{} out of range", lba),
        }
    }
}

#[inline(always)]
fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline(always)]
fn write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// Waits until `done()` holds, or fails after `timeout_us`.
fn wait(what: &'static str, timeout_us: u64, mut done: impl FnMut() -> bool) -> Result<(), SdError> {
    let start = timer::now_us();
    while !done() {
        if timer::now_us() - start > timeout_us {
            return Err(SdError::Timeout(what));
        }
    }
    Ok(())
}

pub struct SdCard {
    /// Relative card address, assigned by the card during identification.
    rca: u32,
    /// SDHC/SDXC cards take block numbers; standard capacity cards take byte offsets.
    high_capacity: bool,
    /// Capacity in blocks, when the CSD version is understood.
    blocks: Option<u64>,
    base_clock: u32,
}

impl SdCard {
    /// Takes over the card slot and brings the card into the transfer state.
    pub fn init() -> Result<Self, SdError> {
        for pin in 48..=53 {
            let pin = Pin::new(pin).expect("valid SD pin");
            set_pin_mode(pin, PinMode::Alt3);
            // CMD and DAT0-3 need pull-ups; CLK (48) is driven by the host.
            set_pin_pull(pin, if pin.number() == 48 { PullMode::Off } else { PullMode::Up });
        }

        let base_clock = query::<GetClockRate>(&Mailbox, ClockId::EMMC).map_err(SdError::Clock)?.hz;
        if base_clock == 0 {
            return Err(SdError::NoClock);
        }
        let mut card = SdCard { rca: 0, high_capacity: false, blocks: None, base_clock };

        write(CONTROL0, 0);
        write(CONTROL1, read(CONTROL1) | SRST_HC);
        wait("controller reset", RESET_TIMEOUT, || read(CONTROL1) & SRST_HC == 0)?;
        write(CONTROL1, read(CONTROL1) | CLK_INTLEN | DATA_TIMEOUT_MAX);
        card.set_clock(IDENTIFICATION_HZ)?;
        // Polled: report every event in INTERRUPT, but raise no IRQs.
        write(IRPT_EN, 0);
        write(IRPT_MASK, INT_ALL);
        write(INTERRUPT, INT_ALL);

        card.command(GO_IDLE_STATE, 0)?;
        // Version 2 cards echo CMD8's check pattern; version 1 cards don't answer at all.
        let version2 = match card.command(SEND_IF_COND, IF_COND_ARG) {
            Ok(response) if response[0] & 0xFFF == IF_COND_ARG => true,
            Ok(response) => return Err(SdError::BadIfCond(response[0])),
            Err(SdError::Command { .. }) => {
                card.reset_command_line()?;
                false
            }
            Err(error) => return Err(error),
        };

        let ocr_request = OCR_VOLTAGE | if version2 { OCR_HCS } else { 0 };
        let start = timer::now_us();
        let ocr = loop {
            card.command(APP_CMD, 0)?;
            let ocr = card.command(SD_SEND_OP_COND, ocr_request)?[0];
            if ocr & OCR_READY != 0 {
                break ocr;
            }
            if timer::now_us() - start > POWER_UP_TIMEOUT {
                return Err(SdError::Timeout("card power-up"));
            }
            timer::delay_us(10_000);
        };
        card.high_capacity = ocr & OCR_HCS != 0;

        card.command(ALL_SEND_CID, 0)?;
        card.rca = card.command(SEND_RELATIVE_ADDR, 0)?[0] & 0xFFFF_0000;
        let csd = card.command(SEND_CSD, card.rca)?;
        card.blocks = sd::capacity_blocks(csd_register(csd));
        card.command(SELECT_CARD, card.rca)?;
        card.set_clock(TRANSFER_HZ)?;
        if !card.high_capacity {
            card.command(SET_BLOCKLEN, BLOCK_SIZE as u32)?;
        }
        Ok(card)
    }

    pub fn high_capacity(&self) -> bool {
        self.high_capacity
    }

    pub fn blocks(&self) -> Option<u64> {
        self.blocks
    }

    /// Sets the SD clock to at most `hz`, divided from the controller's base clock.
    fn set_clock(&mut self, hz: u32) -> Result<(), SdError> {
        wait("idle lines", COMMAND_TIMEOUT, || read(STATUS) & (CMD_INHIBIT | DAT_INHIBIT) == 0)?;
        write(CONTROL1, read(CONTROL1) & !CLK_EN);
        timer::delay_us(10);

        // SDHCI 3.0 has a 10-bit divider N giving base / (2N); older hosts only take
        // powers of two up to 256.
        let spec_version = (read(SLOTISR_VER) >> 16) & 0xFF;
        let mut divider = self.base_clock.div_ceil(2 * hz);
        if spec_version < 2 {
            divider = divider.next_power_of_two().min(0x80);
        }
        let divider = divider.min(0x3FF);
        let bits = (divider & 0xFF) << 8 | (divider >> 8 & 0x3) << 6;
        write(CONTROL1, read(CONTROL1) & !CLK_DIVIDER_MASK | bits);
        timer::delay_us(10);
        wait("clock to stabilise", COMMAND_TIMEOUT, || read(CONTROL1) & CLK_STABLE != 0)?;
        write(CONTROL1, read(CONTROL1) | CLK_EN);
        timer::delay_us(10);
        Ok(())
    }

    fn reset_command_line(&mut self) -> Result<(), SdError> {
        write(CONTROL1, read(CONTROL1) | SRST_CMD);
        wait("command line reset", RESET_TIMEOUT, || read(CONTROL1) & SRST_CMD == 0)
    }

    /// Sends `cmd` and returns the response registers.
    fn command(&mut self, cmd: Command, arg: u32) -> Result<[u32; 4], SdError> {
        let uses_data = cmd.reads_data || cmd.response == ResponseKind::ShortBusy;
        let busy = CMD_INHIBIT | if uses_data { DAT_INHIBIT } else { 0 };
        wait("command line", COMMAND_TIMEOUT, || read(STATUS) & busy == 0)?;

        write(INTERRUPT, INT_ALL);
        write(ARG1, arg);
        write(CMDTM, cmd.encode());
        self.wait_for(cmd, INT_CMD_DONE, "command", COMMAND_TIMEOUT)?;
        if cmd.response == ResponseKind::ShortBusy {
            self.wait_for(cmd, INT_DATA_DONE, "card busy", DATA_TIMEOUT)?;
        }
        Ok(core::array::from_fn(|i| read(RESP0 + i * 4)))
    }

    /// Waits for `flag` in INTERRUPT, failing on any error bit. Clears `flag`.
    fn wait_for(&mut self, cmd: Command, flag: u32, what: &'static str, timeout_us: u64) -> Result<(), SdError> {
        let mut interrupt = 0;
        wait(what, timeout_us, || {
            interrupt = read(INTERRUPT);
            interrupt & (flag | INT_ERROR) != 0
        })?;
        if interrupt & INT_ERROR != 0 {
            write(INTERRUPT, INT_ALL);
            return Err(SdError::Command { index: cmd.index, interrupt: interrupt & 0xFFFF_0000 });
        }
        write(INTERRUPT, flag);
        Ok(())
    }
}

impl BlockDevice for SdCard {
    type Error = SdError;

    fn read_block(&mut self, lba: Lba, block: &mut Block) -> Result<(), SdError> {
        let address = if self.high_capacity { lba.0 } else { lba.0 * BLOCK_SIZE as u64 };
        let address = u32::try_from(address).map_err(|_| SdError::OutOfRange(lba))?;

        write(BLKSIZECNT, 1 << 16 | BLOCK_SIZE as u32);
        self.command(READ_SINGLE_BLOCK, address)?;
        self.wait_for(READ_SINGLE_BLOCK, INT_READ_RDY, "data", DATA_TIMEOUT)?;
        for word in block.chunks_exact_mut(4) {
            word.copy_from_slice(&read(DATA).to_le_bytes());
        }
        self.wait_for(READ_SINGLE_BLOCK, INT_DATA_DONE, "end of data", DATA_TIMEOUT)
    }
}

/// The CSD as the controller presents a 136-bit response: without its CRC byte, so the
/// register's bit n is at bit n - 8 of the response registers.
fn csd_register(response: [u32; 4]) -> u128 {
    let value = (response[3] as u128) << 96 | (response[2] as u128) << 64 | (response[1] as u128) << 32 | response[0] as u128;
    value << 8
}
