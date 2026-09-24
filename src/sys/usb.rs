// usb.rs
//! USB, as far as it goes so far: start the host controller and enumerate the bus, through
//! the hubs. On the Pi 3 B+ that is the LAN7515's two hubs, its LAN7800 Ethernet controller,
//! and whatever is plugged in.

use rustypi_core::usb::tree::{self, Device};
use crate::drivers::usb::{Controller, Host, UsbError};

pub use rustypi_core::usb::tree::Port;

/// What `scan` found.
pub struct Scan {
    pub controller: Controller,
    /// The device on the root port, with everything behind it.
    pub root: Device<UsbError>,
}

#[derive(Debug)]
pub enum ScanError {
    /// Starting the controller.
    Start(UsbError),
    /// Enumerating the device on the root port.
    Root(tree::Error<UsbError>),
}

impl core::fmt::Display for ScanError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            ScanError::Start(error) => write!(f, "{error}"),
            ScanError::Root(error) => write!(f, "root device: {error}"),
        }
    }
}

/// Starts (or restarts, resetting everything on the bus) the host controller and enumerates
/// every device. Takes a second or two, sleeping in between.
pub fn scan() -> Result<Scan, ScanError> {
    let mut host = Host::start().map_err(ScanError::Start)?;
    let speed = host.port.speed;
    let root = tree::enumerate(&mut host, speed).map_err(ScanError::Root)?;
    Ok(Scan { controller: host.controller, root })
}
