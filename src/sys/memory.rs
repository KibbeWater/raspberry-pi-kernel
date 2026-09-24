// memory.rs
//! Physical memory for programs: every page of RAM from the end of the kernel (its image,
//! stack and heap) to the top of the ARM's share, handed out a page at a time. Program
//! pages and page tables come from here, not the kernel heap.

use rustypi_abi::layout::PAGE_SIZE;
use rustypi_core::frames::{FrameStats, Frames};
use rustypi_core::paging::FrameSource;
use crate::drivers::mailbox::{tags, Batch, Mailbox, MailboxError};
use crate::synchronization::{interface::Mutex, IrqLock};

static FRAMES: IrqLock<Option<Frames>> = IrqLock::new(None);

/// The kernel's page frames, for program address spaces.
pub struct PageFrames;

pub static PAGE_FRAMES: PageFrames = PageFrames;

impl FrameSource for PageFrames {
    fn allocate(&self) -> Option<u64> {
        FRAMES.lock(|frames| frames.as_mut()?.allocate())
    }

    unsafe fn free(&self, frame: u64) {
        FRAMES.lock(|frames| {
            let frames = frames.as_mut().expect("frames are only freed once allocated");
            if let Err(error) = frames.free(frame) {
                panic!("freeing page frame {:#x}: {:?}", frame, error);
            }
        });
    }
}

/// Takes over the RAM above the kernel. Returns how many bytes that is.
pub fn init() -> Result<u64, MailboxError> {
    extern "C" {
        static __end: u8;
    }
    let mut batch = Batch::new();
    let memory = batch.add::<tags::GetArmMemory>(());
    let memory = batch.send(&Mailbox)?.get(memory)?;
    let frames = Frames::new(&raw const __end as u64, memory.base as u64 + memory.size as u64);
    let bytes = frames.stats().total as u64 * PAGE_SIZE;
    FRAMES.lock(|slot| *slot = Some(frames));
    Ok(bytes)
}

/// Page frames in total and free, if `init` has run.
pub fn stats() -> FrameStats {
    FRAMES.lock(|frames| frames.as_ref().map_or(FrameStats { total: 0, free: 0 }, Frames::stats))
}
