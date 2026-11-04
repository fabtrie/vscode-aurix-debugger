//! Moving bytes over the probe's bulk pipes.

use crate::Result;

/// A connection to the probe's pipes. [`crate::usb::UsbLink`] is the real
/// one; tests use a scripted link.
pub trait Link {
    /// Writes one transfer to an OUT pipe.
    fn write(&mut self, pipe: u8, data: &[u8]) -> Result<()>;
    /// Reads one transfer of at most `cap` bytes from an IN pipe.
    fn read(&mut self, pipe: u8, cap: usize) -> Result<Vec<u8>>;
}

impl<L: Link + ?Sized> Link for Box<L> {
    fn write(&mut self, pipe: u8, data: &[u8]) -> Result<()> {
        (**self).write(pipe, data)
    }
    fn read(&mut self, pipe: u8, cap: usize) -> Result<Vec<u8>> {
        (**self).read(pipe, cap)
    }
}

/// A link that records what is written and answers reads from a queue.
#[derive(Debug, Default)]
pub struct ScriptedLink {
    pub written: Vec<(u8, Vec<u8>)>,
    pub replies: std::collections::VecDeque<Vec<u8>>,
}

impl Link for ScriptedLink {
    fn write(&mut self, pipe: u8, data: &[u8]) -> Result<()> {
        self.written.push((pipe, data.to_vec()));
        Ok(())
    }
    fn read(&mut self, _pipe: u8, _cap: usize) -> Result<Vec<u8>> {
        self.replies
            .pop_front()
            .ok_or(crate::Error::Usb { op: "ScriptedLink::read", code: 121 })
    }
}
