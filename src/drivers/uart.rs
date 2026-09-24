// UART.rs
use core::cell::UnsafeCell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::board::PERIPHERAL_BASE;
use crate::drivers::gpio::{Pin, PinMode, PullMode, set_pin_mode, set_pin_pull};

const UART0_BASE: usize = PERIPHERAL_BASE + 0x20_1000;

/// Bit flags for the UART Flag Register.
const UART_FR_TXFF: u32 = 1 << 5; // Transmit FIFO full
const UART_FR_RXFE: u32 = 1 << 4; // Receive FIFO empty
const UART_FR_BUSY: u32 = 1 << 3; // Still transmitting

/// Error bits in the Data Register that accompany a damaged received byte:
/// framing (8), parity (9), break (10) and overrun (11).
const UART_DR_ERRORS: u32 = 0xF << 8;

/// Bit flags for the UART Control Register.
const UART_CR_UARTEN: u32 = 1 << 0; // UART enable
const UART_CR_TXE: u32 = 1 << 8;    // Transmit enable
const UART_CR_RXE: u32 = 1 << 9;    // Receive enable

/// Receive and receive-timeout bits, shared by the interrupt mask and clear registers.
/// The timeout fires when bytes sit below the FIFO trigger level for 32 bit periods.
const UART_INT_RX: u32 = 1 << 4;
const UART_INT_RT: u32 = 1 << 6;

/// Bit flags for the UART Line Control Register.
const UART_LCRH_FEN: u32 = 1 << 4;          // FIFO enable
const UART_LCRH_WLEN_8BIT: u32 = 3 << 5;      // 8‑bit word length

/// PL011 reference clock. Pin it with `init_uart_clock=48000000` in config.txt.
const UART_CLOCK_HZ: u32 = 48_000_000;

/// Representation of the UART registers. Note that we only list
/// the registers used in our code. The layout (with reserved words)
/// is taken from the BCM2835 ARM Peripherals manual.
#[repr(C)]
struct UartRegisters {
    dr: u32,         // 0x00: Data Register
    rsrecr: u32,     // 0x04: Receive Status / Error Clear Register
    _reserved0: [u32; 4], // 0x08–0x14: Reserved
    fr: u32,         // 0x18: Flag Register
    _reserved1: u32, // 0x1C: Reserved
    ilpr: u32,       // 0x20: IrDA Low‑Power Register (unused)
    ibrd: u32,       // 0x24: Integer Baud Rate Divisor
    fbrd: u32,       // 0x28: Fractional Baud Rate Divisor
    lcrh: u32,       // 0x2C: Line Control Register
    cr: u32,         // 0x30: Control Register
    _ifls: u32,      // 0x34: Interrupt FIFO Level Select Register (unused)
    imsc: u32,       // 0x38: Interrupt Mask Set Clear Register
    _ris: u32,       // 0x3C: Raw Interrupt Status Register (unused)
    _mis: u32,       // 0x40: Masked Interrupt Status Register (unused)
    icr: u32,        // 0x44: Interrupt Clear Register
}

/// Returns a mutable reference to the UART registers.
///
/// All unsafe pointer conversions are confined here.
#[inline(always)]
fn uart_regs() -> &'static mut UartRegisters {
    unsafe { &mut *(UART0_BASE as *mut UartRegisters) }
}

/// A helper function that wraps a volatile write.
#[inline(always)]
fn write_reg(reg: &mut u32, value: u32) {
    unsafe { write_volatile(reg, value) };
}

/// A helper function that wraps a volatile read.
#[inline(always)]
fn read_reg(reg: &u32) -> u32 {
    unsafe { read_volatile(reg) }
}

/// A zero‑sized type providing a safe UART API.
pub struct Uart;

/// `fmt::Write` adapter for the UART.
pub struct UartWriter;

impl core::fmt::Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Uart::send_string(s);
        Ok(())
    }
}

impl Uart {
    /// Initializes the UART peripheral.
    ///
    /// - Disables the UART while configuring.
    /// - Sets GPIO14 (TX) and GPIO15 (RX) to Alt0 (UART) mode.
    /// - Disables pull‑up/down on those pins via the GPIO module.
    /// - Clears pending interrupts.
    /// - Configures `baud`, 8‑N‑1 (8 bits, no parity, 1 stop bit) with FIFOs enabled.
    /// - Masks all interrupts.
    /// - Finally, enables the UART.
    pub fn init(baud: u32) {
        let uart = uart_regs();

        // Disable UART while configuring.
        write_reg(&mut uart.cr, 0);

        // Configure GPIO pins for UART:
        // Set GPIO14 (TX) and GPIO15 (RX) to Alt0.
        let tx_pin = Pin::new(14).expect("Invalid TX pin");
        let rx_pin = Pin::new(15).expect("Invalid RX pin");
        set_pin_mode(tx_pin, PinMode::Alt0);
        set_pin_mode(rx_pin, PinMode::Alt0);
        // Disable pull‑up/down on TX and RX pins.
        set_pin_pull(tx_pin, PullMode::Off);
        set_pin_pull(rx_pin, PullMode::Off);

        // Clear all pending interrupts.
        write_reg(&mut uart.icr, 0x7FF);

        // Calculate and set baud rate divisors.
        //   divisor = UART_CLOCK_HZ / (16 * baud), split into an integer part and a
        //   6-bit fraction. In 1/64ths that is round(UART_CLOCK_HZ * 4 / baud).
        //   e.g. 115200 -> 26 + 3/64, 38400 -> 78 + 8/64.
        let divisor_x64 = (UART_CLOCK_HZ * 4 + baud / 2) / baud;
        write_reg(&mut uart.ibrd, divisor_x64 >> 6);
        write_reg(&mut uart.fbrd, divisor_x64 & 0x3F);

        // Enable FIFOs and configure 8‑bit word length, 1 stop bit, no parity.
        write_reg(&mut uart.lcrh, UART_LCRH_FEN | UART_LCRH_WLEN_8BIT);

        // Mask all interrupts.
        write_reg(&mut uart.imsc, 0);

        // Enable UART, TX, and RX.
        write_reg(&mut uart.cr, UART_CR_UARTEN | UART_CR_TXE | UART_CR_RXE);
    }

    /// Sends a single byte over UART.
    ///
    /// Waits until there is room in the transmit FIFO.
    pub fn send(c: u8) {
        let uart = uart_regs();
        // Wait while the transmit FIFO is full.
        while (read_reg(&uart.fr) & UART_FR_TXFF) != 0 {}
        write_reg(&mut uart.dr, c as u32);
    }

    /// Waits until every queued byte has left the transmitter.
    pub fn flush() {
        let uart = uart_regs();
        while (read_reg(&uart.fr) & UART_FR_BUSY) != 0 {}
    }

    /// Starts filling the receive queue from the UART interrupt. `Irq::Uart0` must be
    /// enabled too.
    pub fn enable_rx_interrupt() {
        let uart = uart_regs();
        write_reg(&mut uart.imsc, UART_INT_RX | UART_INT_RT);
    }

    /// Moves everything in the receive FIFO into the receive queue.
    pub fn handle_interrupt() {
        let uart = uart_regs();
        while (read_reg(&uart.fr) & UART_FR_RXFE) == 0 {
            RX_QUEUE.push((read_reg(&uart.dr) & 0xFFF) as u16);
        }
        write_reg(&mut uart.icr, UART_INT_RX | UART_INT_RT);
    }

    /// Whether `receive_checked` has something to return.
    pub fn has_input() -> bool {
        RX_QUEUE.has_input()
    }

    /// Takes one received byte from the receive queue.
    ///
    /// Returns `None` if nothing is queued, or `Some(Err(byte))` when the byte is
    /// damaged: the UART flagged it (framing, parity, break or overrun error), or the
    /// queue overflowed and bytes were lost before it.
    pub fn receive_checked() -> Option<Result<u8, u8>> {
        let entry = RX_QUEUE.pop()?;
        let byte = entry as u8;
        Some(if entry as u32 & UART_DR_ERRORS != 0 { Err(byte) } else { Ok(byte) })
    }

    /// Sends a string over UART.
    pub fn send_string(s: &str) {
        for byte in s.bytes() {
            Self::send(byte);
        }
    }
}

const RX_QUEUE_LEN: usize = 1024;

/// Single-producer (IRQ handler), single-consumer (the link task) ring buffer of data
/// register values: the byte plus its error bits.
struct RxQueue {
    entries: UnsafeCell<[u16; RX_QUEUE_LEN]>,
    /// Next slot to write; only the producer stores it.
    head: AtomicUsize,
    /// Next slot to read; only the consumer stores it.
    tail: AtomicUsize,
    /// Set by the producer when the queue was full; the consumer reports it once.
    overflowed: AtomicBool,
}

// Safe because each slot is only written by the producer before it publishes `head`,
// and only read by the consumer before it releases the slot through `tail`.
unsafe impl Sync for RxQueue {}

static RX_QUEUE: RxQueue = RxQueue {
    entries: UnsafeCell::new([0; RX_QUEUE_LEN]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
    overflowed: AtomicBool::new(false),
};

impl RxQueue {
    fn push(&self, entry: u16) {
        let head = self.head.load(Ordering::Relaxed);
        let next = (head + 1) % RX_QUEUE_LEN;
        if next == self.tail.load(Ordering::Acquire) {
            self.overflowed.store(true, Ordering::Relaxed);
            return;
        }
        unsafe { (*self.entries.get())[head] = entry };
        self.head.store(next, Ordering::Release);
    }

    fn pop(&self) -> Option<u16> {
        // Swapped, so an overflow the IRQ handler flags meanwhile isn't lost.
        if self.overflowed.swap(false, Ordering::Relaxed) {
            return Some(UART_DR_ERRORS as u16);
        }
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let entry = unsafe { (*self.entries.get())[tail] };
        self.tail.store((tail + 1) % RX_QUEUE_LEN, Ordering::Release);
        Some(entry)
    }

    fn has_input(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed)
            || self.tail.load(Ordering::Relaxed) != self.head.load(Ordering::Acquire)
    }
}
