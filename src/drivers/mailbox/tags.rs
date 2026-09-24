// tags.rs
//! Firmware property tags, from
//! <https://github.com/raspberrypi/firmware/wiki/Mailbox-property-interface>.

use super::{Tag, Words};

pub struct GetFirmwareRevision;
impl Tag for GetFirmwareRevision {
    const ID: u32 = 0x0000_0001;
    type Request = ();
    type Response = u32;
}

pub struct GetBoardRevision;
impl Tag for GetBoardRevision {
    const ID: u32 = 0x0001_0002;
    type Request = ();
    type Response = u32;
}

/// A range of memory, in bytes.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MemoryRegion {
    pub base: u32,
    pub size: u32,
}

unsafe impl Words for MemoryRegion {}

/// The memory the firmware leaves to the ARM cores; the rest belongs to the GPU.
pub struct GetArmMemory;
impl Tag for GetArmMemory {
    const ID: u32 = 0x0001_0005;
    type Request = ();
    type Response = MemoryRegion;
}

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct SensorId(pub u32);

unsafe impl Words for SensorId {}

impl SensorId {
    pub const SOC: SensorId = SensorId(0);
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Temperature {
    pub id: SensorId,
    pub millidegrees: u32,
}

unsafe impl Words for Temperature {}

pub struct GetTemperature;
impl Tag for GetTemperature {
    const ID: u32 = 0x0003_0006;
    type Request = SensorId;
    type Response = Temperature;
}
