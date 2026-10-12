//! Chunk uploads and bounded directory records observe the originating lease.
use super::error::Error;
use super::lease::Lease;
use rdedup_protocol::object::ListingRecord;
use sgdata::SGData;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct Upload {
    contents: SGData,
    part: usize,
    offset: usize,
    lease: Arc<Lease>,
}
impl Upload {
    pub(super) fn new(contents: SGData, lease: Arc<Lease>) -> Self {
        Self {
            contents,
            part: 0,
            offset: 0,
            lease,
        }
    }
}
impl Read for Upload {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        self.lease.check().map_err(io::Error::from)?;
        while let Some(part) = self.contents.as_parts().get(self.part) {
            if self.offset == part.len() {
                self.part += 1;
                self.offset = 0;
                continue;
            }
            let length = destination.len().min(part.len() - self.offset);
            destination[..length]
                .copy_from_slice(&part[self.offset..self.offset + length]);
            self.offset += length;
            return Ok(length);
        }
        Ok(0)
    }
}

pub(super) fn listing(
    mut reader: impl BufRead,
    mut entry: impl FnMut(PathBuf) -> Result<(), Error>,
) -> Result<(), Error> {
    loop {
        let mut line = Vec::new();
        // JSON escaping can expand the maximum portable path by six times.
        let length = reader
            .by_ref()
            .take(32 * 1024)
            .read_until(b'\n', &mut line)?;
        if length == 0 || line.last() != Some(&b'\n') {
            return Err(Error::Protocol);
        }
        let record = serde_json::from_slice::<ListingRecord>(&line)
            .map_err(|_| Error::Protocol)?;
        match record {
            ListingRecord::Entry { path } => entry(path.to_path_buf())?,
            ListingRecord::Complete => return Ok(()),
            ListingRecord::Failed { problem } => {
                return Err(Error::Problem(problem.kind))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listing_requires_an_explicit_terminal_record() {
        let entry = b"{\"kind\":\"entry\",\"path\":\"0/name/item.yml\"}\n";
        let mut paths = Vec::new();
        assert!(listing(&entry[..], |path| {
            paths.push(path);
            Ok(())
        })
        .is_err());
        assert_eq!(paths, [PathBuf::from("0/name/item.yml")]);
        let mut complete = entry.to_vec();
        complete.extend_from_slice(b"{\"kind\":\"complete\"}\n");
        assert!(listing(&complete[..], |_| Ok(())).is_ok());
        assert!(listing(&vec![b'x'; 64 * 1024][..], |_| Ok(())).is_err());
    }
}
