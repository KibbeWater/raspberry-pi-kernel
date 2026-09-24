// usb.rs
//! USB: a task started at boot brings up the host controller and enumerates the bus, through
//! the hubs (on the Pi 3 B+, the LAN7515's two hubs, its LAN7800 Ethernet controller, and
//! whatever is plugged in). If there is a boot protocol keyboard, the task then polls it and
//! turns what is typed into lines for the shell, echoed on the screen.

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt;
use core::time::Duration;
use rustypi_core::line::LineEditor;
use rustypi_core::usb::tree::{self, Bus, Device, Target};
use rustypi_core::usb::{key_to_char, Direction, EndpointType, KeyboardReport, SetupPacket};
use crate::drivers::usb::{Controller, Host, InterruptIn, Toggle, UsbError};
use crate::synchronization::{interface::Mutex as _, Mutex};
use crate::{println, sched};

pub use rustypi_core::usb::tree::Port;

/// The longest line the keyboard types, like the link's.
const MAX_LINE: usize = rustypi_core::link::MAX_LINE;
/// The keyboard is polled at least this far apart, whatever it asks for: the scheduler's
/// tick, and plenty for typing.
const MIN_POLL: Duration = Duration::from_millis(10);
/// How long to back off after the keyboard fails to answer (unplugged, say).
const ERROR_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum ScanError {
    /// Starting the controller.
    Start(UsbError),
    /// Enumerating the device on the root port.
    Root(tree::Error<UsbError>),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ScanError::Start(error) => write!(f, "{error}"),
            ScanError::Root(error) => write!(f, "root device: {error}"),
        }
    }
}

enum State {
    Starting,
    Failed(ScanError),
    /// The host boxed: it holds its DMA buffer, which then stays put.
    Running { host: Box<Host>, root: Device<UsbError> },
}

/// A sleeping lock: a transfer can take a few milliseconds.
static USB: Mutex<State> = Mutex::new(State::Starting);

/// Where USB is, for `inspect`.
pub enum Status<'a> {
    Starting,
    Failed(&'a ScanError),
    Running { controller: Controller, root: &'a Device<UsbError> },
}

/// Runs `f` with where USB is: still starting, failed, or the bus as enumerated.
pub fn inspect<R>(f: impl FnOnce(Status) -> R) -> R {
    USB.lock(|state| {
        f(match state {
            State::Starting => Status::Starting,
            State::Failed(error) => Status::Failed(error),
            State::Running { host, root } => Status::Running { controller: host.controller, root },
        })
    })
}

/// Starts USB in the background. Lines typed on a keyboard go to `on_line`, without their
/// newline.
pub fn start(on_line: fn(String)) {
    sched::spawn("usb", move || run(on_line));
}

fn run(on_line: fn(String)) {
    // Enumerated before taking the lock, which would otherwise be held for seconds.
    let scanned = Host::start().map_err(ScanError::Start).and_then(|mut host| {
        let speed = host.port.speed;
        let root = tree::enumerate(&mut host, speed).map_err(ScanError::Root)?;
        Ok((host, root))
    });
    let keyboard = match scanned {
        Ok((host, root)) => {
            let keyboard = find_keyboard(&root);
            USB.lock(|state| *state = State::Running { host: Box::new(host), root });
            keyboard
        }
        Err(error) => {
            println!("usb: {error}");
            USB.lock(|state| *state = State::Failed(error));
            return;
        }
    };
    match keyboard {
        Some(keyboard) => serve_keyboard(keyboard, on_line),
        None => println!("usb: no keyboard"),
    }
}

/// A boot protocol keyboard found on the bus.
struct Keyboard {
    target: Target,
    interface: u8,
    input: InterruptIn,
    poll: Duration,
}

fn find_keyboard(root: &Device<UsbError>) -> Option<Keyboard> {
    root.walk().into_iter().find_map(|(_, device)| {
        let interface = device.configuration.interfaces.iter().find(|i| i.is_boot_keyboard())?;
        let endpoint = interface.endpoint(EndpointType::Interrupt, Direction::In)?;
        Some(Keyboard {
            target: device.target(),
            interface: interface.number,
            input: InterruptIn { target: device.target(), endpoint: endpoint.number, max_packet: endpoint.max_packet_size },
            // Full and low speed intervals are in milliseconds (frames).
            poll: Duration::from_millis(endpoint.interval as u64).max(MIN_POLL),
        })
    })
}

/// Runs `f` with the host controller, if USB is running.
fn with_host<R>(f: impl FnOnce(&mut Host) -> Result<R, UsbError>) -> Option<Result<R, UsbError>> {
    USB.lock(|state| match state {
        State::Running { host, .. } => Some(f(host)),
        _ => None,
    })
}

/// Sets the keyboard up for boot protocol reports, then polls it for good, typing into a
/// line editor.
fn serve_keyboard(keyboard: Keyboard, on_line: fn(String)) {
    // Boot protocol: the simple 8-byte reports, whatever its own report format is. And a
    // report only when something changes, if it can (SET_IDLE is optional, and some
    // keyboards stall it: then it just reports more often).
    let setup = with_host(|host| {
        host.control(keyboard.target, SetupPacket::set_protocol(keyboard.interface, true), &mut [])?;
        let _ = host.control(keyboard.target, SetupPacket::set_idle(keyboard.interface), &mut []);
        Ok(())
    });
    if let Some(Err(error)) = setup {
        println!("usb: keyboard: {error}");
        return;
    }
    println!("usb: keyboard at address {} ready", keyboard.target.address);

    let mut toggle = Toggle::new();
    let mut previous = KeyboardReport { modifiers: 0, keys: [0; 6] };
    let mut editor = LineEditor::new(MAX_LINE);
    let mut failing = false;
    loop {
        sched::sleep(keyboard.poll);
        let mut bytes = [0; KeyboardReport::LENGTH];
        let polled = with_host(|host| host.interrupt_in(keyboard.input, &mut toggle, &mut bytes));
        let report = match polled {
            Some(Ok(Some(count))) => KeyboardReport::parse(&bytes[..count]).ok(),
            Some(Ok(None)) => None,
            Some(Err(error)) => {
                // Said once, not every poll, until it answers again.
                if !failing {
                    println!("usb: keyboard: {error}");
                    failing = true;
                }
                sched::sleep(ERROR_BACKOFF);
                continue;
            }
            None => return,
        };
        failing = false;
        let Some(report) = report else { continue };
        for key in report.pressed_since(&previous) {
            let Some(c) = key_to_char(key, report.shift()) else { continue };
            if let Some(line) = editor.feed(c, |shown| super::console::write_fmt(format_args!("{shown}"))) {
                on_line(line);
            }
        }
        previous = report;
    }
}
