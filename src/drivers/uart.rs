// UART.rs
use core::ptr::{read_volatile, write_volatile};
use crate::drivers::gpio::{Pin, PinMode, PullMode, set_pin_mode, set_pin_pull};

/// Base address for UART0 on the Raspberry Pi 3B+.
const UART0_BASE: usize = 0x3F201000;

/// Bit flags for the UART Flag Register.
const UART_FR_TXFF: u32 = 1 << 5; // Transmit FIFO full
const UART_FR_RXFE: u32 = 1 << 4; // Receive FIFO empty

/// Bit flags for the UART Control Register.
const UART_CR_UARTEN: u32 = 1 << 0; // UART enable
const UART_CR_TXE: u32 = 1 << 8;    // Transmit enable
const UART_CR_RXE: u32 = 1 << 9;    // Receive enable

/// Bit flags for the UART Line Control Register.
const UART_LCRH_FEN: u32 = 1 << 4;          // FIFO enable
const UART_LCRH_WLEN_8BIT: u32 = 3 << 5;      // 8‑bit word length

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

impl Uart {
    /// Initializes the UART peripheral.
    ///
    /// - Disables the UART while configuring.
    /// - Sets GPIO14 (TX) and GPIO15 (RX) to Alt0 (UART) mode.
    /// - Disables pull‑up/down on those pins via the GPIO module.
    /// - Clears pending interrupts.
    /// - Configures 115200 baud, 8‑N‑1 (8 bits, no parity, 1 stop bit) with FIFOs enabled.
    /// - Masks all interrupts.
    /// - Finally, enables the UART.
    pub fn init() {
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

        // Calculate and set baud rate divisors for 115200 baud.
        // With a UART clock of 48MHz:
        //   divisor = 48000000 / (16 * 115200) ≈ 26.0416666...
        //   -> Integer part: 26, Fractional part: round(0.0416666 * 64) = 3.
        write_reg(&mut uart.ibrd, 26);
        write_reg(&mut uart.fbrd, 3);

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

    /// Tries to receive one byte from UART.
    ///
    /// Returns `Some(u8)` if data is available, or `None` if the receive FIFO is empty.
    pub fn receive() -> Option<u8> {
        let uart = uart_regs();
        if (read_reg(&uart.fr) & UART_FR_RXFE) != 0 {
            None
        } else {
            Some((read_reg(&uart.dr) & 0xFF) as u8)
        }
    }

    /// Sends a string over UART.
    pub fn send_string(s: &str) {
        for byte in s.bytes() {
            Self::send(byte);
        }
    }

    /// (Optional) Blocking version of receive.
    ///
    /// Waits until a byte is received and then returns it.
    pub fn receive_blocking() -> u8 {
        loop {
            if let Some(byte) = Self::receive() {
                return byte;
            }
        }
    }
}
