// property.rs
//! Typed property interface: each firmware tag is a type carrying its ID and its request and
//! response layouts, so a request can't be paired with the wrong tag and a response can't be
//! read at the wrong offset.
//!
//! ```ignore
//! let temperature = mailbox::query::<GetTemperature>(SensorId::SOC)?;
//!
//! let mut batch = Batch::new();
//! let revision = batch.add::<GetBoardRevision>(());
//! let memory = batch.add::<GetArmMemory>(());
//! let replies = batch.send()?;
//! let memory = replies.get(memory)?;
//! ```
//!
//! Message layout: `[size, code, (tag, value size, tag code, value words...)*, end]`.

use core::fmt;
use core::marker::PhantomData;
use core::ptr;
use super::{call, Channel, Message, MESSAGE_WORDS};

const REQUEST: u32 = 0;
const RESPONSE_OK: u32 = 0x8000_0000;
/// Set in a tag's code once the firmware has answered it; the low bits are the response
/// length in bytes.
const TAG_RESPONSE: u32 = 1 << 31;
const END_TAG: u32 = 0;
/// Words before a tag's value: ID, value buffer size, tag code.
const TAG_HEADER_WORDS: usize = 3;

/// Types that are nothing but 32-bit words, so they can be copied into and out of a message
/// buffer as-is.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]` or `#[repr(transparent)]`, consist only of `u32` fields
/// (or other `Words` types) with no padding, and be valid for every bit pattern.
pub unsafe trait Words: Copy {}

unsafe impl Words for () {}
unsafe impl Words for u32 {}
unsafe impl<const N: usize> Words for [u32; N] {}

/// Number of words `T` occupies. Checked at compile time for each type it is used with.
const fn words_of<T: Words>() -> usize {
    const { assert!(size_of::<T>() % 4 == 0 && align_of::<T>() <= 4, "Words type must be whole u32s") };
    size_of::<T>() / 4
}

/// A firmware property tag.
pub trait Tag {
    const ID: u32;
    type Request: Words;
    type Response: Words;
}

/// An address in the GPU's view of memory, as the firmware hands out (e.g. a framebuffer).
/// It has to be converted before the ARM cores can use it.
#[allow(dead_code)] // Handed out by framebuffer tags, which come next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct BusAddress(pub u32);

unsafe impl Words for BusAddress {}

#[allow(dead_code)]
impl BusAddress {
    /// The same memory as seen from the ARM cores: the bus address without its cache alias
    /// bits (the top two).
    pub fn to_arm(self) -> usize {
        (self.0 & 0x3FFF_FFFF) as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MailboxError {
    /// The firmware rejected the whole message.
    Firmware,
    /// The firmware left this tag unanswered, usually because it doesn't know it.
    Unanswered { tag: u32 },
    /// The response needed more room than the tag's types allow for.
    Truncated { tag: u32, needed: usize, capacity: usize },
    /// The response was shorter than the tag's response type.
    ShortResponse { tag: u32, len: usize, expected: usize },
    /// Too many tags for one message.
    BatchFull,
    /// The handle came from a different batch.
    WrongBatch { tag: u32 },
}

impl fmt::Display for MailboxError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            MailboxError::Firmware => write!(f, "firmware rejected the message"),
            MailboxError::Unanswered { tag } => write!(f, "tag {:#x} not answered", tag),
            MailboxError::Truncated { tag, needed, capacity } => {
                write!(f, "tag {:#x} response needs {} bytes, room for {}", tag, needed, capacity)
            }
            MailboxError::ShortResponse { tag, len, expected } => {
                write!(f, "tag {:#x} response is {} bytes, expected {}", tag, len, expected)
            }
            MailboxError::BatchFull => write!(f, "too many tags for one message"),
            MailboxError::WrongBatch { tag } => write!(f, "tag {:#x} handle is from another batch", tag),
        }
    }
}

/// Refers to one tag in a `Batch`, and reads its response from the `Replies`.
pub struct Handle<T: Tag> {
    /// Word index of the tag's header, or `usize::MAX` if it didn't fit.
    offset: usize,
    _tag: PhantomData<T>,
}

impl<T: Tag> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Tag> Copy for Handle<T> {}

/// Several tags sent to the firmware in one message. Some settings only take effect together,
/// like the framebuffer's size, depth and allocation.
pub struct Batch {
    msg: Message,
    /// Words used so far, including the two-word message header.
    len: usize,
    full: bool,
}

impl Batch {
    pub fn new() -> Self {
        Batch { msg: Message([0; MESSAGE_WORDS]), len: 2, full: false }
    }

    /// Adds a tag. If the message is full, `send` fails with `BatchFull`.
    pub fn add<T: Tag>(&mut self, request: T::Request) -> Handle<T> {
        let value_words = words_of::<T::Request>().max(words_of::<T::Response>());
        let offset = self.len;
        // Leave room for the end tag.
        if offset + TAG_HEADER_WORDS + value_words + 1 > MESSAGE_WORDS {
            self.full = true;
            return Handle { offset: usize::MAX, _tag: PhantomData };
        }

        let buf = &mut self.msg.0;
        buf[offset] = T::ID;
        buf[offset + 1] = (value_words * 4) as u32;
        buf[offset + 2] = REQUEST;
        let value = offset + TAG_HEADER_WORDS;
        buf[value..value + value_words].fill(0);
        // `Words` guarantees the request is plain u32s. Copied as bytes, since `()` has no
        // u32 alignment to rely on.
        unsafe {
            ptr::copy_nonoverlapping(
                &request as *const T::Request as *const u8,
                buf.as_mut_ptr().add(value) as *mut u8,
                size_of::<T::Request>(),
            );
        }

        self.len = value + value_words;
        Handle { offset, _tag: PhantomData }
    }

    pub fn send(mut self) -> Result<Replies, MailboxError> {
        if self.full {
            return Err(MailboxError::BatchFull);
        }
        let buf = &mut self.msg.0;
        buf[self.len] = END_TAG;
        buf[0] = ((self.len + 1) * 4) as u32;
        buf[1] = REQUEST;

        call(Channel::Property, &mut self.msg);

        if self.msg.0[1] != RESPONSE_OK {
            return Err(MailboxError::Firmware);
        }
        Ok(Replies { msg: self.msg, len: self.len })
    }
}

/// The firmware's answers to a sent `Batch`.
pub struct Replies {
    msg: Message,
    len: usize,
}

impl Replies {
    pub fn get<T: Tag>(&self, handle: Handle<T>) -> Result<T::Response, MailboxError> {
        let buf = &self.msg.0;
        let offset = handle.offset;
        if offset.saturating_add(TAG_HEADER_WORDS) > self.len || buf[offset] != T::ID {
            return Err(MailboxError::WrongBatch { tag: T::ID });
        }

        let code = buf[offset + 2];
        if code & TAG_RESPONSE == 0 {
            return Err(MailboxError::Unanswered { tag: T::ID });
        }
        let len = (code & !TAG_RESPONSE) as usize;
        let capacity = buf[offset + 1] as usize;
        let expected = size_of::<T::Response>();
        if len > capacity {
            return Err(MailboxError::Truncated { tag: T::ID, needed: len, capacity });
        }
        if len < expected {
            return Err(MailboxError::ShortResponse { tag: T::ID, len, expected });
        }

        // `Words` guarantees any bit pattern is a valid response, and the value buffer holds
        // at least `words_of::<T::Response>()` words.
        Ok(unsafe { ptr::read(buf.as_ptr().add(offset + TAG_HEADER_WORDS) as *const T::Response) })
    }
}

/// Sends a single tag and returns its response.
#[allow(dead_code)] // Part of the API; everything so far batches.
pub fn query<T: Tag>(request: T::Request) -> Result<T::Response, MailboxError> {
    let mut batch = Batch::new();
    let handle = batch.add::<T>(request);
    batch.send()?.get(handle)
}
