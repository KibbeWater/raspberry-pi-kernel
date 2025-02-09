use core::ptr::{read_volatile, write_volatile};

/// Base address for the Raspberry Pi 3 GPIO registers.
const GPIO_BASE: usize = 0x3F200000;

/// Represents the memory‑mapped registers for the GPIO peripheral.
///
/// Only the registers needed for this example are defined.
#[repr(C)]
struct GpioRegisters {
    /// Function Select registers (GPFSEL0–GPFSEL5)
    gpfsel: [u32; 6],
    _reserved0: u32,
    /// GPIO Pin Output Set registers (GPSET0, GPSET1)
    gpset: [u32; 2],
    _reserved1: u32,
    /// GPIO Pin Output Clear registers (GPCLR0, GPCLR1)
    gpclr: [u32; 2],
    // Other registers are omitted for brevity.
}

impl GpioRegisters {
    /// Returns a mutable reference to the GPIO registers.
    ///
    /// # Safety
    /// This function encapsulates the unsafe conversion from a raw pointer
    /// to a mutable reference. It is safe to use if you trust that the
    /// hardware is mapped at `GPIO_BASE` and that no data races occur.
    #[inline(always)]
    fn instance() -> &'static mut Self {
        unsafe { &mut *(GPIO_BASE as *mut Self) }
    }
}

/// A type representing a valid GPIO pin for the Raspberry Pi 3.
/// Valid GPIO pin numbers are 0 through 53.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    number: u8,
}

impl Pin {
    /// Creates a new GPIO Pin if `number` is in range (0..54), otherwise returns `None`.
    pub fn new(number: u8) -> Option<Self> {
        if number < 54 {
            Some(Pin { number })
        } else {
            None
        }
    }

    /// Returns the underlying pin number.
    pub fn number(self) -> u8 {
        self.number
    }
}

/// The different function modes a GPIO pin can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinMode {
    Input = 0,
    Output = 1,
    Alt0 = 4,
    Alt1 = 5,
    Alt2 = 6,
    Alt3 = 7,
    Alt4 = 3,
    Alt5 = 2,
}

/// Configures the function (mode) for a given GPIO pin.
///
/// Each GPFSEL register controls 10 pins, with 3 bits per pin.
pub fn set_pin_mode(pin: Pin, mode: PinMode) {
    let num = pin.number() as usize;
    let gpio = GpioRegisters::instance();

    let reg_index = num / 10;        // Determine which GPFSEL register.
    let bit = (num % 10) * 3;          // Each pin uses 3 bits.

    unsafe {
        let current = read_volatile(&gpio.gpfsel[reg_index]);
        let mask = !(0b111 << bit);  // Clear the 3 bits for this pin.
        let new_val = (current & mask) | ((mode as u32) << bit);
        write_volatile(&mut gpio.gpfsel[reg_index], new_val);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helper functions for directly setting and clearing a pin.
// They are not public, so users of the library cannot accidentally misuse them.
// ─────────────────────────────────────────────────────────────────────────────

fn set_pin_internal(pin: Pin) {
    let num = pin.number() as usize;
    let gpio = GpioRegisters::instance();
    let reg_index = num / 32;
    let bit = num % 32;

    unsafe {
        write_volatile(&mut gpio.gpset[reg_index], 1 << bit);
    }
}

fn clear_pin_internal(pin: Pin) {
    let num = pin.number() as usize;
    let gpio = GpioRegisters::instance();
    let reg_index = num / 32;
    let bit = num % 32;

    unsafe {
        write_volatile(&mut gpio.gpclr[reg_index], 1 << bit);
    }
}

/// Sets the output state of a GPIO pin.
///
/// Instead of having separate functions for setting and clearing a pin,
/// this function takes a boolean value:
/// - `true` sets (turns ON) the pin.
/// - `false` clears (turns OFF) the pin.
///
/// # Example
///
/// ```rust
/// # use your_gpio_module::{Pin, PinMode, set_pin_mode, write_pin};
/// // Assume that pin 16 is connected to an LED.
/// let led = Pin::new(16).expect("Invalid GPIO pin");
/// set_pin_mode(led, PinMode::Output);
/// write_pin(led, true);  // Turn LED on.
/// write_pin(led, false); // Turn LED off.
/// ```
pub fn write_pin(pin: Pin, state: bool) {
    if state {
        set_pin_internal(pin);
    } else {
        clear_pin_internal(pin);
    }
}
