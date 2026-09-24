// usb.rs
//! USB.

use rustypi_core::session::{LineKind, Reply};
use rustypi_core::usb::tree::Device;
use rustypi_core::usb::{Class, Speed};
use super::{Command, Outcome, Shell};
use crate::drivers::usb::UsbError;
use crate::sys;
use crate::sys::usb::{Port, Status};

pub const COMMANDS: &[Command] = &[
    Command { name: "usb", args: "", description: "list the devices on the USB bus, as found at boot", run: usb },
];

fn usb<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    sys::usb::inspect(|status| match status {
        Status::Starting => reply.rsp("usb: still starting"),
        Status::Failed(error) => reply.line(LineKind::Rsp, format_args!("usb: {error}")),
        Status::Running { controller, root } => {
            let version = controller.version;
            reply.line(LineKind::Rsp, format_args!(
                "controller: DWC2 {:x}.{:02x}{}, {} host channels",
                version >> 12,
                version >> 4 & 0xFF,
                char::from_digit((version & 0xF) as u32, 16).unwrap_or('?'),
                controller.channels,
            ));
            list(root, "", reply);
        }
    });
    Outcome::Done
}

fn speed_name(speed: Speed) -> &'static str {
    match speed {
        Speed::Low => "low",
        Speed::Full => "full",
        Speed::High => "high",
    }
}

/// A device's line, then its hub's ports' lines indented under it. `at` starts its line: where
/// it is, like `port 2: `, indented as deep as its hub.
fn list(device: &Device<UsbError>, at: &str, reply: &mut Reply) {
    let class = match device.descriptor.class {
        Class::PER_INTERFACE => device.configuration.interfaces.first().map_or(Class::PER_INTERFACE, |i| i.class),
        class => class,
    };
    let hub = device.hub.as_ref().map(|hub| alloc::format!(", {} ports", hub.descriptor.ports)).unwrap_or_default();
    let split = device
        .translator
        .map(|translator| alloc::format!(", split through hub {} port {}", translator.hub, translator.port))
        .unwrap_or_default();
    reply.line(LineKind::Rsp, format_args!(
        "{}{} {:04x}:{:04x} {}{}, {} speed{}",
        at,
        device.address,
        device.descriptor.vendor,
        device.descriptor.product,
        class.name(),
        hub,
        speed_name(device.speed),
        split,
    ));
    let Some(hub) = &device.hub else { return };
    let child_at = alloc::format!("{}  ", " ".repeat(at.len()));
    for (i, port) in hub.ports.iter().enumerate() {
        let port_at = alloc::format!("{}port {}: ", child_at, i + 1);
        match port {
            Port::Empty => reply.line(LineKind::Rsp, format_args!("{port_at}empty")),
            Port::Device(child) => list(child, &port_at, reply),
            Port::Failed(error) => reply.line(LineKind::Rsp, format_args!("{port_at}failed: {error}")),
        }
    }
}
