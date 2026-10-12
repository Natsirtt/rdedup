//! CLI publication sources distinguish single-use streams from replayable input.
use rdedup_lib::backends::ProtectionError;
use rdedup_lib::publication::WriteRetryPolicy;
use rdedup_lib::{EncryptHandle, Repo, WriteStats};
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use tempfile::NamedTempFile;

#[derive(Debug, clap::Args)]
pub(super) struct Arguments {
    /// Read a file and reopen it after lease loss; keep its contents unchanged
    #[clap(long, value_name = "FILE", conflicts_with = "spool")]
    input: Option<PathBuf>,
    /// Spool stdin to temporary disk before uploading, allowing automatic replay
    #[clap(long)]
    spool: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("could not spool publication input: {0}")]
    Spool(#[source] io::Error),
    #[error("could not publish input: {0}")]
    Publication(#[source] io::Error),
    #[error("stdin publication lost its lease and cannot replay; repeat with --input FILE or --spool: {0}")]
    StreamInterrupted(#[source] io::Error),
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        let kind = match &error {
            Error::Spool(source)
            | Error::Publication(source)
            | Error::StreamInterrupted(source) => source.kind(),
        };
        Self::new(kind, error)
    }
}

pub(super) enum Input<Stream> {
    Streaming(Stream),
    File(PathBuf),
    Spooled(NamedTempFile),
}

impl Arguments {
    pub(super) fn prepare<Stream: Read>(
        self,
        mut stream: Stream,
    ) -> Result<Input<Stream>, Error> {
        if let Some(path) = self.input {
            return Ok(Input::File(path));
        }
        if self.spool {
            let mut spool = NamedTempFile::new().map_err(Error::Spool)?;
            io::copy(&mut stream, &mut spool).map_err(Error::Spool)?;
            return Ok(Input::Spooled(spool));
        }
        Ok(Input::Streaming(stream))
    }
}

impl<Stream: Read + Send> Input<Stream> {
    pub(super) fn write(
        self,
        repository: &Repo,
        name: &str,
        encryption: &EncryptHandle,
    ) -> Result<WriteStats, Error> {
        let policy = WriteRetryPolicy::default();
        match self {
            Self::Streaming(stream) => {
                return repository.write(name, stream, encryption).map_err(
                    |error| {
                        if ProtectionError::find_in(&error).is_some() {
                            Error::StreamInterrupted(error)
                        } else {
                            Error::Publication(error)
                        }
                    },
                );
            }
            Self::File(path) => repository.write_replayable(
                name,
                &mut || File::open(&path),
                encryption,
                policy,
            ),
            Self::Spooled(spool) => repository.write_replayable(
                name,
                &mut || spool.reopen(),
                encryption,
                policy,
            ),
        }
        .map_err(Error::Publication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spooled_input_reopens_at_byte_zero_and_cleans_up() {
        let input = Arguments {
            input: None,
            spool: true,
        }
        .prepare(&b"complete publication"[..])
        .unwrap();
        let Input::Spooled(spool) = input else {
            panic!("explicit spooling must produce an owned temporary source");
        };
        let path = spool.path().to_owned();
        let mut first = spool.reopen().unwrap();
        let mut prefix = [0; 3];
        first.read_exact(&mut prefix).unwrap();
        let mut second = spool.reopen().unwrap();
        let mut contents = Vec::new();
        second.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"complete publication");
        contents.clear();
        first.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"plete publication");
        drop(first);
        drop(second);
        drop(spool);
        assert!(!path.exists());
    }
}
