// mailbox/mod.rs
//! Typed VideoCore mailbox property interface: each firmware tag is a type carrying its ID
//! and its request and response layouts, so a request can't be paired with the wrong tag
//! and a response can't be read at the wrong offset.
//!
//! ```ignore
//! let temperature = mailbox::query::<GetTemperature>(&Mailbox, SensorId::SOC)?;
//!
//! let mut batch = Batch::new();
//! let revision = batch.add::<GetBoardRevision>(());
//! let memory = batch.add::<GetArmMemory>(());
//! let replies = batch.send(&Mailbox)?;
//! let memory = replies.get(memory)?;
//! ```
//!
//! Message layout: `[size, code, (tag, value size, tag code, value words...)*, end]`.
//! Delivering a message is left to a `Transport`: the kernel's hardware mailbox, or a
//! fake firmware in tests.

pub mod tags;

use core::fmt;
use core::marker::PhantomData;
use core::ptr;

/// Size of a message buffer in 32-bit words.
pub const MESSAGE_WORDS: usize = 256;

/// A message buffer. The firmware needs 16-byte alignment, since the low four bits of the
/// address carry the channel. Only `Batch` can create one, so every message a `Transport`
/// sees is well formed.
#[repr(C, align(16))]
pub struct Message([u32; MESSAGE_WORDS]);

impl Message {
    /// The buffer, for handing its address to the firmware.
    pub fn as_mut_ptr(&mut self) -> *mut u32 {
        self.0.as_mut_ptr()
    }
}

/// Delivers a property message to the firmware and waits for it to answer in place.
pub trait Transport {
    fn call(&self, msg: &mut Message);
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct BusAddress(pub u32);

unsafe impl Words for BusAddress {}

impl BusAddress {
    /// The same memory as seen from the ARM cores: the bus address without its cache alias
    /// bits (the top two).
    pub fn to_arm(self) -> usize {
        (self.0 & 0x3FFF_FFFF) as usize
    }

    /// Memory at ARM address `addr` as a DMA engine (like the USB controller's) must be given
    /// it: through the alias that bypasses the GPU's L2 cache, since the ARM cores don't see
    /// that cache. The ARM cores' own caches are the caller's business.
    pub fn from_arm(addr: usize) -> Self {
        BusAddress((addr & 0x3FFF_FFFF) as u32 | 0xC000_0000)
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
            MailboxError::Unanswered { tag } => write!(f, "tag {tag:#x} not answered"),
            MailboxError::Truncated { tag, needed, capacity } => {
                write!(f, "tag {tag:#x} response needs {needed} bytes, room for {capacity}")
            }
            MailboxError::ShortResponse { tag, len, expected } => {
                write!(f, "tag {tag:#x} response is {len} bytes, expected {expected}")
            }
            MailboxError::BatchFull => write!(f, "too many tags for one message"),
            MailboxError::WrongBatch { tag } => write!(f, "tag {tag:#x} handle is from another batch"),
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

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn send(mut self, transport: &impl Transport) -> Result<Replies, MailboxError> {
        if self.full {
            return Err(MailboxError::BatchFull);
        }
        let buf = &mut self.msg.0;
        buf[self.len] = END_TAG;
        buf[0] = ((self.len + 1) * 4) as u32;
        buf[1] = REQUEST;

        transport.call(&mut self.msg);

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
pub fn query<T: Tag>(transport: &impl Transport, request: T::Request) -> Result<T::Response, MailboxError> {
    let mut batch = Batch::new();
    let handle = batch.add::<T>(request);
    batch.send(transport)?.get(handle)
}

#[cfg(test)]
mod tests {
    use super::tags::*;
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// Answers each tag with `respond(id, request value words)`, or leaves it unanswered on
    /// `None`, like the firmware. Remembers the last message it was sent.
    type Respond = Box<dyn Fn(u32, &[u32]) -> Option<Vec<u32>>>;

    struct FakeFirmware {
        respond: Respond,
        last: RefCell<Vec<u32>>,
    }

    impl FakeFirmware {
        fn new(respond: impl Fn(u32, &[u32]) -> Option<Vec<u32>> + 'static) -> Self {
            FakeFirmware { respond: Box::new(respond), last: RefCell::new(Vec::new()) }
        }
    }

    impl Transport for FakeFirmware {
        fn call(&self, msg: &mut Message) {
            let buf = &mut msg.0;
            let words = buf[0] as usize / 4;
            *self.last.borrow_mut() = buf[..words].to_vec();
            assert_eq!(buf[1], REQUEST);
            assert_eq!(buf[words - 1], END_TAG);
            let mut i = 2;
            while buf[i] != END_TAG {
                let (id, capacity) = (buf[i], buf[i + 1] as usize / 4);
                assert_eq!(buf[i + 2], REQUEST);
                let value = i + TAG_HEADER_WORDS;
                if let Some(response) = (self.respond)(id, &buf[value..value + capacity]) {
                    for (k, word) in response.iter().take(capacity).enumerate() {
                        buf[value + k] = *word;
                    }
                    buf[i + 2] = TAG_RESPONSE | (response.len() * 4) as u32;
                }
                i = value + capacity;
            }
            buf[1] = RESPONSE_OK;
        }
    }

    fn board() -> FakeFirmware {
        FakeFirmware::new(|id, request| match id {
            GetBoardRevision::ID => Some(vec![0xa020d3]),
            GetFirmwareRevision::ID => Some(vec![1733399223]),
            GetArmMemory::ID => Some(vec![0, 948 << 20]),
            GetTemperature::ID => Some(vec![request[0], 37_000]),
            _ => None,
        })
    }

    #[test]
    fn encodes_a_batch_and_decodes_each_reply() {
        let firmware = board();
        let mut batch = Batch::new();
        let revision = batch.add::<GetBoardRevision>(());
        let memory = batch.add::<GetArmMemory>(());
        let temperature = batch.add::<GetTemperature>(SensorId(7));
        let replies = batch.send(&firmware).unwrap();

        assert_eq!(*firmware.last.borrow(), [
            17 * 4, REQUEST,
            GetBoardRevision::ID, 4, REQUEST, 0,
            GetArmMemory::ID, 8, REQUEST, 0, 0,
            // The value buffer fits the larger of request and response.
            GetTemperature::ID, 8, REQUEST, 7, 0,
            END_TAG,
        ]);
        assert_eq!(replies.get(revision).unwrap(), 0xa020d3);
        assert_eq!(replies.get(memory).unwrap().size, 948 << 20);
        let temperature = replies.get(temperature).unwrap();
        assert_eq!((temperature.id.0, temperature.millidegrees), (7, 37_000));
    }

    #[test]
    fn query_sends_a_single_tag() {
        assert_eq!(query::<GetFirmwareRevision>(&board(), ()).unwrap(), 1733399223);
    }

    #[test]
    fn reports_unanswered_truncated_and_short_responses() {
        let silent = FakeFirmware::new(|_, _| None);
        assert!(matches!(
            query::<GetBoardRevision>(&silent, ()),
            Err(MailboxError::Unanswered { tag: GetBoardRevision::ID }),
        ));

        let chatty = FakeFirmware::new(|_, _| Some(vec![1, 2, 3]));
        assert!(matches!(
            query::<GetBoardRevision>(&chatty, ()),
            Err(MailboxError::Truncated { needed: 12, capacity: 4, .. }),
        ));

        let terse = FakeFirmware::new(|_, _| Some(vec![]));
        assert!(matches!(
            query::<GetArmMemory>(&terse, ()),
            Err(MailboxError::ShortResponse { len: 0, expected: 8, .. }),
        ));
    }

    #[test]
    fn firmware_rejecting_the_message_is_an_error() {
        struct Rejecting;
        impl Transport for Rejecting {
            fn call(&self, msg: &mut Message) {
                msg.0[1] = 0x8000_0001;
            }
        }
        assert!(matches!(query::<GetBoardRevision>(&Rejecting, ()), Err(MailboxError::Firmware)));
    }

    #[test]
    fn handles_from_another_batch_are_caught() {
        let firmware = board();
        let mut first = Batch::new();
        let _ = first.add::<GetBoardRevision>(());
        let first = first.send(&firmware).unwrap();

        let mut second = Batch::new();
        let _ = second.add::<GetArmMemory>(());
        let temperature = second.add::<GetTemperature>(SensorId::SOC);
        let _ = second.send(&firmware).unwrap();

        assert!(matches!(first.get(temperature), Err(MailboxError::WrongBatch { .. })));
    }

    #[test]
    fn overfull_batches_fail_to_send() {
        let mut batch = Batch::new();
        for _ in 0..60 {
            let _ = batch.add::<GetArmMemory>(());
        }
        assert!(matches!(batch.send(&board()), Err(MailboxError::BatchFull)));
    }

    #[test]
    fn bus_addresses_drop_their_cache_alias() {
        assert_eq!(BusAddress(0xC3C0_0000).to_arm(), 0x03C0_0000);
        assert_eq!(BusAddress(0x3C00_0000).to_arm(), 0x3C00_0000);
    }

    #[test]
    fn arm_addresses_get_the_uncached_alias_for_dma() {
        assert_eq!(BusAddress::from_arm(0x0010_0040), BusAddress(0xC010_0040));
        assert_eq!(BusAddress::from_arm(0x0010_0040).to_arm(), 0x0010_0040);
    }

    #[test]
    fn power_states_read_both_ways() {
        let request = tags::PowerState::on(tags::DeviceId::USB_HCD);
        assert_eq!((request.device.0, request.state), (3, 0b11));
        assert!(tags::PowerState { device: tags::DeviceId::USB_HCD, state: 1 }.is_on());
        assert!(!tags::PowerState { device: tags::DeviceId(99), state: 0b10 }.is_on());
        assert!(!tags::PowerState { device: tags::DeviceId::USB_HCD, state: 0 }.is_on());
    }

    #[test]
    fn mac_addresses_fill_two_words() {
        assert_eq!(size_of::<tags::MacAddress>(), 8);
    }
}
