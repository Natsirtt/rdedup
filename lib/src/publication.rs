//! Replayable publication restarts the whole protected write, never its tail.
use crate::backends::ProtectionError;
use crate::{EncryptHandle, Repo, WriteStats};
use std::io::{self, Read};
use std::num::NonZeroU32;

/// A source that can open a fresh reader for the same complete byte sequence.
///
/// Every successful `open` must start at the beginning and yield identical bytes.
/// The source must keep its contents stable for the duration of publication.
/// A factory returning a reader implements this trait directly.
pub trait ReplayableSource {
    /// An independently owned reader consumed by one complete write attempt.
    type Reader: Read + Send;

    /// Open the complete source after repository protection has been acquired.
    ///
    /// # Errors
    /// Returns a source failure without publishing a name or retrying that failure.
    fn open(&mut self) -> io::Result<Self::Reader>;
}
impl<Factory, Reader> ReplayableSource for Factory
where
    Factory: FnMut() -> io::Result<Reader>,
    Reader: Read + Send,
{
    type Reader = Reader;
    fn open(&mut self) -> io::Result<Reader> {
        self()
    }
}

/// A nonzero bound on complete write attempts, including the first attempt.
#[derive(Clone, Copy, Debug)]
pub struct WriteRetryPolicy {
    max_attempts: NonZeroU32,
}
impl WriteRetryPolicy {
    /// Limit how many fresh protected writes may be attempted for one publication.
    pub fn new(max_attempts: NonZeroU32) -> Self {
        Self { max_attempts }
    }
}
impl Default for WriteRetryPolicy {
    fn default() -> Self {
        Self::new(NonZeroU32::new(3).expect("three attempts is nonzero"))
    }
}

impl Repo {
    /// Publish a stable source, reopening it after loss of operation protection.
    ///
    /// Each attempt acquires a new shared operation before opening the source.
    /// Failed attempts finish their workers and release their original protection
    /// before another attempt begins. Uploaded chunks remain reusable; only a
    /// successful complete write publishes or confirms the name's identity.
    ///
    /// # Errors
    /// Source failures, conflicts, authentication and ordinary I/O failures return
    /// immediately. Only typed protection loss permits a whole-write retry, bounded
    /// by `policy`; exhausted retries return the final protection failure.
    pub fn write_replayable<Source: ReplayableSource>(
        &self,
        name: &str,
        source: &mut Source,
        encryption: &EncryptHandle,
        policy: WriteRetryPolicy,
    ) -> io::Result<WriteStats> {
        for attempt in 0..policy.max_attempts.get() {
            let result = match self.backend.begin_shared() {
                Ok(operation) => {
                    let reader = source.open()?;
                    self.write_attempt(name, reader, encryption, operation)
                }
                Err(error) => Err(error),
            };
            match result {
                Err(error)
                    if attempt + 1 < policy.max_attempts.get()
                        && ProtectionError::find_in(&error).is_some() =>
                {
                    continue
                }
                result => return result,
            }
        }
        unreachable!("a nonzero attempt bound always returns its final result")
    }
}
