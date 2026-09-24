// usb.rs
//! USB: a task started at boot brings up the host controller and enumerates the bus, through
//! the hubs (on the Pi 3 B+, the LAN7515's two hubs, its LAN7800 Ethernet controller, and
//! whatever is plugged in). It then watches the hubs for devices plugged in or pulled out,
//! and polls the boot protocol keyboard if there is one: what is typed is echoed on the
//! screen and goes to the shell a line at a time, in the layout chosen with `set_layout`
//! (saved in `/keyboard.txt`), with accents, key repeat, and Ctrl+C, Ctrl+U and Ctrl+L.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use rustypi_core::keys::{Composer, Repeater};
use rustypi_core::line::LineEditor;
use rustypi_core::usb::tree::{self, Bus, Change, Device, Target, Tree};
use rustypi_core::usb::{Direction, EndpointType, KeyboardReport, Modifiers, SetupPacket, Typed};
use crate::drivers::timer;
use crate::drivers::usb::{Controller, Host, InterruptIn, Toggle, UsbError};
use crate::synchronization::{interface::Mutex as _, Mutex};
use crate::sys::fs::{self, FsError};
use crate::{println, sched};

pub use rustypi_core::usb::tree::Port;
pub use rustypi_core::usb::Layout;

/// The longest line the keyboard types, like the link's.
const MAX_LINE: usize = rustypi_core::link::MAX_LINE;
/// The keyboard is polled at least this far apart, whatever it asks for: the scheduler's
/// tick, and plenty for typing.
const MIN_POLL: Duration = Duration::from_millis(10);
/// How often the hubs are asked whether anything was plugged in or pulled out.
const HOTPLUG_POLL: Duration = Duration::from_millis(500);
/// How long to back off after the keyboard fails to answer (unplugged, say).
const ERROR_BACKOFF: Duration = Duration::from_secs(1);

/// Where the keyboard layout is kept between boots: its name, like `sv`.
const LAYOUT_FILE: &str = "/keyboard.txt";

/// The keyboard layout, as an index into `Layout::ALL`.
static LAYOUT: AtomicUsize = AtomicUsize::new(0);

/// The keyboard layout in use.
pub fn layout() -> Layout {
    Layout::ALL[LAYOUT.load(Ordering::Relaxed)]
}

/// Switches the keyboard layout, now and (saved in `LAYOUT_FILE`) from the next boot on.
pub fn set_layout(layout: Layout) -> Result<(), FsError> {
    use_layout(layout);
    fs::write_file(LAYOUT_FILE, format!("{}\n", layout.name()).as_bytes())
}

fn use_layout(layout: Layout) {
    let index = Layout::ALL.iter().position(|&l| l == layout).expect("every layout is in ALL");
    LAYOUT.store(index, Ordering::Relaxed);
}

/// Picks up the layout saved by `set_layout`, if there is one.
fn load_layout() {
    let saved = fs::read_file(LAYOUT_FILE).ok().and_then(|bytes| {
        let name = core::str::from_utf8(&bytes).ok()?.trim().to_ascii_lowercase();
        Layout::from_name(&name)
    });
    if let Some(layout) = saved {
        use_layout(layout);
    }
}

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
    Running { host: Box<Host>, tree: Tree<UsbError> },
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
            State::Running { host, tree } => Status::Running { controller: host.controller, root: &tree.root },
        })
    })
}

/// What the keyboard hands the shell.
pub enum Input {
    /// A line typed, without its newline.
    Line(String),
    /// Ctrl+C: stop the program in the foreground.
    Interrupt,
}

/// Starts USB in the background. What is typed on a keyboard goes to `on_input`.
pub fn start(on_input: fn(Input)) {
    sched::spawn("usb", move || run(on_input));
}

fn run(on_input: fn(Input)) {
    load_layout();
    // Enumerated before taking the lock, which would otherwise be held for seconds.
    let scanned = Host::start().map_err(ScanError::Start).and_then(|mut host| {
        let speed = host.port.speed;
        let tree = tree::enumerate(&mut host, speed).map_err(ScanError::Root)?;
        Ok((host, tree))
    });
    match scanned {
        Ok((host, tree)) => USB.lock(|state| *state = State::Running { host: Box::new(host), tree }),
        Err(error) => {
            println!("usb: {error}");
            USB.lock(|state| *state = State::Failed(error));
            return;
        }
    }
    serve(on_input);
}

/// Runs `f` with the host controller and the bus, if USB is running.
fn with_bus<R>(f: impl FnOnce(&mut Host, &mut Tree<UsbError>) -> R) -> Option<R> {
    USB.lock(|state| match state {
        State::Running { host, tree } => Some(f(host, tree)),
        _ => None,
    })
}

/// Polls the keyboard, if there is one, and every `HOTPLUG_POLL` the hubs, for devices
/// plugged in or pulled out: for good.
fn serve(on_input: fn(Input)) {
    let mut keyboard: Option<KeyboardState> = None;
    // A keyboard that wouldn't set up, left alone until it is unplugged.
    let mut refused: Option<u8> = None;
    let mut next_hotplug = 0;
    loop {
        let now = timer::now_us();
        if now >= next_hotplug {
            next_hotplug = now + HOTPLUG_POLL.as_micros() as u64;
            let changes = with_bus(|host, tree| {
                let changes = tree.poll_changes(host);
                for change in &changes {
                    report(tree, *change);
                }
                changes
            })
            .unwrap_or_default();
            for change in changes {
                if let Change::Removed { address } = change {
                    if keyboard.as_ref().is_some_and(|k| k.keyboard.target.address == address) {
                        keyboard = None;
                        println!("usb: keyboard gone");
                    }
                    if refused == Some(address) {
                        refused = None;
                    }
                }
            }
            if keyboard.is_none() {
                let found = with_bus(|_, tree| find_keyboard(&tree.root, refused)).flatten();
                if let Some(found) = found {
                    match set_up(&found) {
                        Ok(()) => {
                            println!("usb: keyboard at address {} ready", found.target.address);
                            keyboard = Some(KeyboardState::new(found));
                        }
                        Err(error) => {
                            println!("usb: keyboard: {error}");
                            refused = Some(found.target.address);
                        }
                    }
                }
            }
        }
        let poll = keyboard.as_ref().map_or(HOTPLUG_POLL, |k| k.keyboard.poll);
        sched::sleep(poll);
        if let Some(state) = keyboard.as_mut() {
            state.poll(on_input);
        }
    }
}

/// Says what came or went.
fn report(tree: &Tree<UsbError>, change: Change) {
    match change {
        Change::Added { address } => {
            if let Some(device) = tree.device(address) {
                println!(
                    "usb: {} {:04x}:{:04x} {} connected",
                    address,
                    device.descriptor.vendor,
                    device.descriptor.product,
                    device.class().name(),
                );
            }
        }
        Change::Removed { address } => println!("usb: {address} disconnected"),
    }
}

/// A boot protocol keyboard found on the bus.
struct Keyboard {
    target: Target,
    interface: u8,
    input: InterruptIn,
    poll: Duration,
}

/// The first boot protocol keyboard on the bus, other than at `skip`.
fn find_keyboard(root: &Device<UsbError>, skip: Option<u8>) -> Option<Keyboard> {
    root.walk().into_iter().filter(|(_, device)| Some(device.address) != skip).find_map(|(_, device)| {
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

/// Switches a keyboard to boot protocol reports: the simple 8-byte ones, whatever its own
/// report format is. And a report only when something changes, if it can (SET_IDLE is
/// optional, and some keyboards stall it: then it just reports more often).
fn set_up(keyboard: &Keyboard) -> Result<(), UsbError> {
    with_bus(|host, _| {
        host.control(keyboard.target, SetupPacket::set_protocol(keyboard.interface, true), &mut [])?;
        let _ = host.control(keyboard.target, SetupPacket::set_idle(keyboard.interface), &mut []);
        Ok(())
    })
    .unwrap_or(Ok(()))
}

/// A keyboard in use, and what typing on it has built up.
struct KeyboardState {
    keyboard: Keyboard,
    toggle: Toggle,
    /// The last report: the keys held, as far as we know.
    previous: KeyboardReport,
    editor: LineEditor,
    composer: Composer,
    repeater: Repeater,
    /// Whether it failed to answer, so an error is said once rather than every poll.
    failing: bool,
}

impl KeyboardState {
    fn new(keyboard: Keyboard) -> Self {
        KeyboardState {
            keyboard,
            toggle: Toggle::new(),
            previous: KeyboardReport { modifiers: 0, keys: [0; 6] },
            editor: LineEditor::new(MAX_LINE),
            composer: Composer::new(),
            repeater: Repeater::new(),
            failing: false,
        }
    }

    /// Asks the keyboard for a report, types what was pressed, and repeats a held key.
    fn poll(&mut self, on_input: fn(Input)) {
        let mut bytes = [0; KeyboardReport::LENGTH];
        let polled = with_bus(|host, _| host.interrupt_in(self.keyboard.input, &mut self.toggle, &mut bytes));
        match polled {
            Some(Ok(Some(count))) => {
                self.failing = false;
                if let Ok(report) = KeyboardReport::parse(&bytes[..count]) {
                    let now = timer::now_us();
                    let pressed: alloc::vec::Vec<u8> = report.pressed_since(&self.previous).collect();
                    for key in pressed {
                        if self.type_key(key, report.held(), on_input) {
                            self.repeater.pressed(key, now);
                        } else {
                            self.repeater.stop();
                        }
                    }
                    self.previous = report;
                }
            }
            Some(Ok(None)) => self.failing = false,
            Some(Err(error)) => {
                // Unplugging is noticed by the hot-plug poll; until then, say it once.
                if !self.failing {
                    println!("usb: keyboard: {error}");
                    self.failing = true;
                }
                sched::sleep(ERROR_BACKOFF);
                return;
            }
            None => return,
        }
        if let Some(key) = self.repeater.due(&self.previous.keys, timer::now_us()) {
            self.type_key(key, self.previous.held(), on_input);
        }
    }

    /// Types what a key gives. Returns whether it repeats when held.
    fn type_key(&mut self, key: u8, modifiers: Modifiers, on_input: fn(Input)) -> bool {
        let Some(typed) = layout().key(key, modifiers) else { return false };
        let editor = &mut self.editor;
        let mut edit = |c: char| {
            if let Some(line) = editor.feed(c, echo) {
                on_input(Input::Line(line));
            }
        };
        match typed {
            Typed::Char(c) => {
                self.composer.char(c, &mut edit);
                true
            }
            Typed::Dead(accent) => {
                self.composer.dead(accent, &mut edit);
                false
            }
            Typed::Ctrl('c') => {
                self.composer.reset();
                self.editor.clear(|_| {});
                echo("^C\n");
                on_input(Input::Interrupt);
                false
            }
            Typed::Ctrl('u') => {
                self.composer.reset();
                self.editor.clear(echo);
                false
            }
            Typed::Ctrl('l') => {
                super::console::clear();
                echo(self.editor.line());
                false
            }
            Typed::Ctrl(_) => false,
        }
    }
}

/// Shows typing on the screen (the link gets the lines, as commands, not keystrokes).
fn echo(shown: &str) {
    super::console::write_fmt(format_args!("{shown}"));
}
