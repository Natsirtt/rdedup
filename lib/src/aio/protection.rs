//! Typed loss of repository-operation protection, independent of transport.
use std::error::Error as StdError;
use std::io;

/// A protected operation can no longer safely continue with its original guard.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Protection was lost; a new operation must restart from its source.
    #[error("repository operation lost its protection")]
    Lost {
        /// The backend's reason, retained through worker and repository errors.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}
impl Error {
    /// Find a typed protection failure through backend and worker I/O wrappers.
    /// Ordinary interrupted reads are not evidence that a repository lease was lost.
    pub fn find_in(error: &io::Error) -> Option<&Self> {
        let mut current: &(dyn StdError + 'static) = error;
        loop {
            if let Some(protection) = current.downcast_ref::<Self>() {
                return Some(protection);
            }
            current = if let Some(error) = current.downcast_ref::<io::Error>() {
                error
                    .get_ref()
                    .map(|inner| inner as &(dyn StdError + 'static))
            } else {
                current.source()
            }?;
        }
    }
}
impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        io::Error::new(io::ErrorKind::Interrupted, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loss_remains_distinguishable_through_nested_io_context() {
        let lost = io::Error::from(Error::Lost {
            source: Box::new(io::Error::from(io::ErrorKind::TimedOut)),
        });
        let nested = io::Error::other(lost);
        assert!(Error::find_in(&nested).is_some());
        assert!(Error::find_in(&io::Error::from(io::ErrorKind::Interrupted))
            .is_none());
    }
}
