// usb.rs
//! USB.

use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "usb", args: "", description: "start the USB controller and describe the root device", run: usb },
];

fn usb<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    let probe = match sys::usb::probe() {
        Ok(probe) => probe,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("usb: {error}"));
            return Outcome::Done;
        }
    };
    let version = probe.controller.version;
    reply.line(LineKind::Rsp, format_args!(
        "controller: DWC2 {:x}.{:02x}{}, {} host channels",
        version >> 12,
        version >> 4 & 0xFF,
        char::from_digit((version & 0xF) as u32, 16).unwrap_or('?'),
        probe.controller.channels,
    ));
    let device = probe.device;
    reply.line(LineKind::Rsp, format_args!(
        "root device: {:04x}:{:04x}, class {}, {:?} speed, endpoint 0 packets of {}",
        device.vendor,
        device.product,
        device.class.0,
        probe.speed,
        device.max_packet_size,
    ));
    if let Some(hub) = probe.hub {
        reply.line(LineKind::Rsp, format_args!("hub: {} ports, power good after {} ms", hub.ports, hub.power_on_to_good_ms));
    }
    Outcome::Done
}
