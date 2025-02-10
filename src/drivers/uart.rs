// uart.rs
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use crate::drivers::gpio::{set_pin_mode, set_pin_pull, Pin, PinMode, PullMode};

//
// UART register bit flags.
//
const UART_FR_TXFF: u32 = 1 << 5;  // Transmit FIFO full
const UART_FR_RXFE: u32 = 1 << 4;  // Receive FIFO empty

const UART_CR_UARTEN: u32 = 1 << 0;  // UART Enable
const UART_CR_TXE: u32 = 1 << 8;     // Transmit Enable
const UART_CR_RXE: u32 = 1 << 9;     // Receive Enable

const UART_LCRH_FEN: u32 = 1 << 4;   // FIFO Enable
const UART_LCRH_WLEN_8BIT: u32 = 3 << 5;  // 8‑bit word length

//
// A small wrapper for a memory–mapped I/O cell that guarantees
// that reads and writes are done with volatile semantics.
//
#[repr(transparent)]
pub struct MmioCell<T> {
    value: UnsafeCell<T>,
}

impl<T> MmioCell<T> {
    pub fn read(&self) -> T
    where
        T: Copy,
    {
        unsafe { core::ptr::read_volatile(self.value.get()) }
    }
    pub fn write(&self, value: T) {
        unsafe { core::ptr::write_volatile(self.value.get(), value) }
    }
}

unsafe impl<T> Sync for MmioCell<T> {}

///
/// This structure represents the UART registers.
///
#[repr(C)]
pub struct UartRegisters {
    pub dr:    MmioCell<u32>,  // Data register (offset 0x00)
    _reserved0: [u32; 5],
    pub fr:    MmioCell<u32>,  // Flag register (offset 0x18)
    _reserved1: [u32; 2],
    pub ibrd:  MmioCell<u32>,  // Integer Baud rate divisor (offset 0x24)
    pub fbrd:  MmioCell<u32>,  // Fractional Baud rate divisor (offset 0x28)
    pub lcrh:  MmioCell<u32>,  // Line control register (offset 0x2C)
    pub cr:    MmioCell<u32>,  // Control register (offset 0x30)
    _reserved2: u32,
    pub imsc:  MmioCell<u32>,  // Interrupt mask set/clear (offset 0x38)
    _reserved3: [u32; 2],
    pub icr:   MmioCell<u32>,  // Interrupt clear register (offset 0x44)
}

// UART0 base address for the Raspberry Pi 3B+.
const UART0_BASE: usize = 0x3F201000;

/// Returns a safe reference to the UART registers.
fn uart() -> &'static UartRegisters {
    unsafe { &*(UART0_BASE as *const UartRegisters) }
}

/// Configures the GPIO pins used by the UART:
/// - TX: GPIO 14  
/// - RX: GPIO 15  
///
/// This function sets both pins to Alt0 mode and disables pull-ups/downs.
fn configure_uart_pins() {
    let tx_pin = Pin::new(14).expect("Invalid TX pin");
    let rx_pin = Pin::new(15).expect("Invalid RX pin");
    set_pin_mode(tx_pin, PinMode::Alt0);
    set_pin_mode(rx_pin, PinMode::Alt0);
    set_pin_pull(tx_pin, PullMode::Off);
    set_pin_pull(rx_pin, PullMode::Off);
}

/// Initializes the UART.
pub fn uart_init() {
    let uart = uart();

    // Disable UART while configuring.
    uart.cr.write(0);

    // Configure the GPIO pins used by UART.
    configure_uart_pins();

    // Clear all pending interrupts.
    uart.icr.write(0x7FF);

    // Set baud rate divisors for 115200 baud (assuming 48MHz UART clock).
    uart.ibrd.write(26);
    uart.fbrd.write(3);

    // Enable FIFO and configure for 8‑bit word length.
    uart.lcrh.write(UART_LCRH_FEN | UART_LCRH_WLEN_8BIT);

    // Mask all interrupts.
    uart.imsc.write(0);

    // Enable UART, transmit, and receive.
    uart.cr.write(UART_CR_UARTEN | UART_CR_TXE | UART_CR_RXE);
}

/// Sends a single byte over the UART.
pub fn uart_send(c: u8) {
    let uart = uart();
    // Wait until there is space in the transmit FIFO.
    while (uart.fr.read() & UART_FR_TXFF) != 0 {
        spin_loop();
    }
    uart.dr.write(c as u32);
}

/// Receives a single byte from the UART if available.
/// Returns `Some(u8)` when data is available or `None` otherwise.
pub fn uart_receive() -> Option<u8> {
    let uart = uart();
    if (uart.fr.read() & UART_FR_RXFE) != 0 {
        None
    } else {
        Some((uart.dr.read() & 0xFF) as u8)
    }
}

/// Receives a single byte from the UART, blocking until data is available.
pub fn uart_read_blocking() -> u8 {
    loop {
        if let Some(byte) = uart_receive() {
            return byte;
        }
        // Optionally, you could add a small delay here.
        spin_loop();
    }
}

/// Reads a line of text from the UART into the provided buffer.
/// This function blocks until a newline (`'\n'`) or carriage return (`'\r'`) is received.
/// It returns the number of bytes read (which may be less than the buffer size).
pub fn uart_read_line(buffer: &mut [u8]) -> &str {
    let mut index = 0;

    loop {
        if let Some(byte) = uart_receive() {
            match byte {
                b'\r' | b'\n' => {
                    uart_send(b'\n'); // Echo newline
                    break; // Stop reading on Enter
                }
                8 | 127 => { // Handle backspace (8 = ASCII Backspace, 127 = Delete)
                    if index > 0 {
                        index -= 1;
                        uart_send(b'\x08'); // Move cursor back
                        uart_send(b' ');    // Erase character
                        uart_send(b'\x08'); // Move cursor back again
                    }
                }
                _ => {
                    if index < buffer.len() - 1 { // Prevent buffer overflow
                        buffer[index] = byte;
                        index += 1;
                        uart_send(byte); // Echo character
                    }
                }
            }
        }
    }

    buffer[index] = 0; // Null-terminate for safety
    core::str::from_utf8(&buffer[..index]).unwrap_or("[Invalid UTF-8]")
}


/// Sends a string over the UART.
pub fn uart_send_string(s: &str) {
    for &c in s.as_bytes() {
        uart_send(c);
    }
}
