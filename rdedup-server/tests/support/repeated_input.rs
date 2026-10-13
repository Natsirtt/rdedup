use std::io::{self, Read, Write};
use std::sync::Arc;

pub struct Input {
    pattern: Arc<[u8]>,
    length: u64,
    position: u64,
}

impl Input {
    pub fn new(pattern: Arc<[u8]>, length: u64) -> Self {
        assert!(!pattern.is_empty());
        Self {
            pattern,
            length,
            position: 0,
        }
    }
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = usize::try_from(
            (self.length - self.position)
                .min(u64::try_from(buffer.len()).unwrap()),
        )
        .unwrap();
        let mut filled = 0;
        while filled < count {
            let offset = usize::try_from(
                self.position % u64::try_from(self.pattern.len()).unwrap(),
            )
            .unwrap();
            let part = (count - filled).min(self.pattern.len() - offset);
            buffer[filled..filled + part]
                .copy_from_slice(&self.pattern[offset..offset + part]);
            self.position += u64::try_from(part).unwrap();
            filled += part;
        }
        Ok(count)
    }
}

pub struct Verifier {
    pattern: Arc<[u8]>,
    position: u64,
}

impl Verifier {
    pub fn new(pattern: Arc<[u8]>) -> Self {
        Self {
            pattern,
            position: 0,
        }
    }

    pub fn bytes_verified(&self) -> u64 {
        self.position
    }
}

impl Write for Verifier {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut verified = 0;
        while verified < buffer.len() {
            let offset = usize::try_from(
                self.position % u64::try_from(self.pattern.len()).unwrap(),
            )
            .unwrap();
            let count =
                (buffer.len() - verified).min(self.pattern.len() - offset);
            assert_eq!(
                &buffer[verified..verified + count],
                &self.pattern[offset..offset + count],
                "restored bytes differ at offset {}",
                self.position
            );
            self.position += u64::try_from(count).unwrap();
            verified += count;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
