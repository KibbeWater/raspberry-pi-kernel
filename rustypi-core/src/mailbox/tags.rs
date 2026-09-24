// tags.rs
//! Firmware property tags, from
//! <https://github.com/raspberrypi/firmware/wiki/Mailbox-property-interface>.

use super::{BusAddress, Tag, Words};

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

/// A clock in the SoC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ClockId(pub u32);

unsafe impl Words for ClockId {}

impl ClockId {
    /// The EMMC controller's base clock, which the SD card clock is divided from.
    pub const EMMC: ClockId = ClockId(1);
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ClockRate {
    pub id: ClockId,
    pub hz: u32,
}

unsafe impl Words for ClockRate {}

pub struct GetClockRate;
impl Tag for GetClockRate {
    const ID: u32 = 0x0003_0002;
    type Request = ClockId;
    type Response = ClockRate;
}

// Framebuffer. The set tags respond with what the firmware actually applied, which can
// differ from the request.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

unsafe impl Words for Size {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Offset {
    pub x: u32,
    pub y: u32,
}

unsafe impl Words for Offset {}

/// Bits per pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Depth(pub u32);

unsafe impl Words for Depth {}

/// Order of the colour channels within a pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PixelOrder(pub u32);

unsafe impl Words for PixelOrder {}

impl PixelOrder {
    pub const BGR: PixelOrder = PixelOrder(0);
    pub const RGB: PixelOrder = PixelOrder(1);
}

/// Byte alignment requested for the framebuffer.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Alignment(pub u32);

unsafe impl Words for Alignment {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FramebufferAllocation {
    pub base: BusAddress,
    pub size: u32,
}

unsafe impl Words for FramebufferAllocation {}

/// The display's resolution, or the configured default when none is attached.
pub struct GetPhysicalSize;
impl Tag for GetPhysicalSize {
    const ID: u32 = 0x0004_0003;
    type Request = ();
    type Response = Size;
}

pub struct SetPhysicalSize;
impl Tag for SetPhysicalSize {
    const ID: u32 = 0x0004_8003;
    type Request = Size;
    type Response = Size;
}

/// Size of the drawable buffer, which can be larger than the display for panning.
pub struct SetVirtualSize;
impl Tag for SetVirtualSize {
    const ID: u32 = 0x0004_8004;
    type Request = Size;
    type Response = Size;
}

pub struct SetVirtualOffset;
impl Tag for SetVirtualOffset {
    const ID: u32 = 0x0004_8009;
    type Request = Offset;
    type Response = Offset;
}

pub struct SetDepth;
impl Tag for SetDepth {
    const ID: u32 = 0x0004_8005;
    type Request = Depth;
    type Response = Depth;
}

pub struct SetPixelOrder;
impl Tag for SetPixelOrder {
    const ID: u32 = 0x0004_8006;
    type Request = PixelOrder;
    type Response = PixelOrder;
}

/// Allocates the framebuffer with the settings from the same message.
pub struct AllocateBuffer;
impl Tag for AllocateBuffer {
    const ID: u32 = 0x0004_0001;
    type Request = Alignment;
    type Response = FramebufferAllocation;
}

/// Bytes per row of the allocated framebuffer.
pub struct GetPitch;
impl Tag for GetPitch {
    const ID: u32 = 0x0004_0008;
    type Request = ();
    type Response = u32;
}
