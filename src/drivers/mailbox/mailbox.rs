const MAILBOX_BASE: usize = 0x3F_B88000;

#[repr(C)]
#[derive(Clone, Copy)]
struct MailboxMessage {
    data: u32,
}

impl MailboxMessage {
    pub fn channel(&self) -> u8 {
        (self.data >> 28) as u8
    }

    pub fn data(&self) -> u32 {
        self.data & 0x0FFFFFFF
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MailboxStatus {
    data: u32,
}

impl MailboxStatus {
    pub fn reserved(&self) -> u32 {
        self.data >> 2
    }

    pub fn empty(&self) -> bool {
        (self.data & 0b10) != 0
    }

    pub fn full(&self) -> bool {
        (self.data & 0b01) != 0
    }
}

#[repr(C)]
struct Mailbox {
    message: MailboxMessage,
    status: MailboxStatus,
}

impl Mailbox {
    #[inline(always)]
    fn mailbox_instance() -> &'static mut Mailbox {
        unsafe { &mut *(MAILBOX_BASE as *mut Mailbox) }
    }
}

pub fn mailbox_read_nonblock() -> Option<MailboxMessage> {
    let mailbox = Mailbox::mailbox_instance();
    
    if !mailbox.status.empty() {
        let message = mailbox.message;
        return Some(message);
    }
    
    None
}

pub fn mailbox_read() -> MailboxMessage {
    let mailbox = Mailbox::mailbox_instance();
    
    loop {
        let status = &mailbox.status;
        
        if !status.empty() {
            let message = mailbox.message;
            return message;
        }
    }
}
