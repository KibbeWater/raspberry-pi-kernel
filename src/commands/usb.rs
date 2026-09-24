// usb.rs
//! USB.

use rustypi_core::session::{LineKind, Reply};
use rustypi_core::usb::tree::Device;
use rustypi_core::usb::{Class, Speed};
use super::{Command, Outcome, Shell};
use crate::drivers::usb::UsbError;
use crate::sys;
use crate::sys::usb::Port;

pub const COMMANDS: &[Command] = &[
    Command { name: "usb", args: "", description: "start the USB controller and list the devices on the bus", run: usb },
];

fn usb<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    let scan = match sys::usb::scan() {
        Ok(scan) => scan,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("usb: {error}"));
            return Outcome::Done;
        }
    };
    let version = scan.controller.version;
    reply.line(LineKind::Rsp, format_args!(
        "controller: DWC2 {:x}.{:02x}{}, {} host channels",
        version >> 12,
        version >> 4 & 0xFF,
        char::from_digit((version & 0xF) as u32, 16).unwrap_or('?'),
        scan.controller.channels,
    ));
    list(&scan.root, "", reply);
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
    reply.line(LineKind::Rsp, format_args!(
        "{}{} {:04x}:{:04x} {}{}, {} speed",
        at,
        device.address,
        device.descriptor.vendor,
        device.descriptor.product,
        class.name(),
        hub,
        speed_name(device.speed),
    ));
    let Some(hub) = &device.hub else { return };
    let child_at = alloc::format!("{}  ", " ".repeat(at.len()));
    for (i, port) in hub.ports.iter().enumerate() {
        let port_at = alloc::format!("{}port {}: ", child_at, i + 1);
        match port {
            Port::Empty => {}
            Port::Device(child) => list(child, &port_at, reply),
            Port::NeedsSplit(speed) => reply.line(LineKind::Rsp, format_args!(
                "{}{} speed device, needs split transactions (not yet)",
                port_at,
                speed_name(*speed),
            )),
            Port::Failed(error) => reply.line(LineKind::Rsp, format_args!("{port_at}failed: {error}")),
        }
    }
}
