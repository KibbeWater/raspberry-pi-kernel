use core::ptr::{read_volatile, write_volatile};
use crate::os::gpio::{Pin, PinMode, set_pin_mode};

// Base addresses for Raspberry Pi 3B+
const UART0_BASE: usize = 0x3F201000;
const GPIO_BASE: usize = 0x3F200000;
const GPPUD: usize = GPIO_BASE + 0x94;
const GPPUDCLK0: usize = GPIO_BASE + 0x98;

// UART registers
const UART_DR: usize = UART0_BASE + 0x00;
const UART_FR: usize = UART0_BASE + 0x18;
const UART_IBRD: usize = UART0_BASE + 0x24;
const UART_FBRD: usize = UART0_BASE + 0x28;
const UART_LCRH: usize = UART0_BASE + 0x2C;
const UART_CR: usize = UART0_BASE + 0x30;
const UART_IMSC: usize = UART0_BASE + 0x38;
const UART_ICR: usize = UART0_BASE + 0x44;  // Added Interrupt Clear Register

// Status flags
const UART_FR_TXFF: u32 = 1 << 5;  // Transmit FIFO full
const UART_FR_RXFE: u32 = 1 << 4;  // Receive FIFO empty

// Control bits
const UART_CR_UARTEN: u32 = 1 << 0;  // UART Enable
const UART_CR_TXE: u32 = 1 << 8;     // Transmit Enable
const UART_CR_RXE: u32 = 1 << 9;     // Receive Enable

// Line control bits
const UART_LCRH_FEN: u32 = 1 << 4;   // FIFO Enable
const UART_LCRH_WLEN_8BIT: u32 = 3 << 5;  // 8-bit word length

fn mmio_write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) };
}

fn mmio_read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn delay(count: u32) {
    for _ in 0..count {
        unsafe { core::arch::asm!("nop") };
    }
}

pub fn uart_init() {
    // Disable UART before configuring
    mmio_write(UART_CR, 0);

    // Set GPIO 14 (TX) and GPIO 15 (RX) to Alt0 mode for UART0
    let tx_pin = Pin::new(14).unwrap();
    let rx_pin = Pin::new(15).unwrap();
    set_pin_mode(tx_pin, PinMode::Alt0);
    set_pin_mode(rx_pin, PinMode::Alt0);

    // Disable pull-up/down for GPIO 14 and 15
    mmio_write(GPPUD, 0);
    delay(150);  // Wait for control signal
    mmio_write(GPPUDCLK0, (1 << 14) | (1 << 15));
    delay(150);  // Hold time for control signal
    mmio_write(GPPUDCLK0, 0);

    // Clear all pending interrupts
    mmio_write(UART_ICR, 0x7FF);

    // Calculate baud rate divisors for 115200 baud
    // UART clock = 48MHz
    // Divisor = UART clock / (16 * baud rate)
    // 48000000 / (16 * 115200) = 26.0416666...
    mmio_write(UART_IBRD, 26);  // Integer part
    mmio_write(UART_FBRD, 3);   // Fractional part (0.0416666... * 64 = 2.67, rounded to 3)

    // Enable FIFOs and 8-bit word length, 1 stop bit, no parity
    mmio_write(UART_LCRH, UART_LCRH_FEN | UART_LCRH_WLEN_8BIT);

    // Mask all interrupts
    mmio_write(UART_IMSC, 0);

    // Enable UART, transmit and receive
    mmio_write(UART_CR, UART_CR_UARTEN | UART_CR_TXE | UART_CR_RXE);
}

pub fn uart_send(c: u8) {
    // Wait until there's space in the transmit FIFO
    while (mmio_read(UART_FR) & UART_FR_TXFF) != 0 {}
    mmio_write(UART_DR, c as u32);
}

pub fn uart_receive() -> Option<u8> {
    // Check if receive FIFO is empty
    if (mmio_read(UART_FR) & UART_FR_RXFE) != 0 {
        None
    } else {
        Some((mmio_read(UART_DR) & 0xFF) as u8)
    }
}

pub fn uart_send_string(s: &str) {
    for c in s.bytes() {
        uart_send(c);
    }
}