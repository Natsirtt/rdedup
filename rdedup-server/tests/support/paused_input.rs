use std::io::{self, Cursor, Read};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// One attempt pauses midway so an exclusive lease can interrupt publication.
pub struct Input {
    bytes: Cursor<Arc<[u8]>>,
    pause: Option<Pause>,
}

pub struct Pause {
    reached: tokio::sync::oneshot::Sender<()>,
    resume: mpsc::Receiver<()>,
}

impl Pause {
    pub fn new(
        reached: tokio::sync::oneshot::Sender<()>,
        resume: mpsc::Receiver<()>,
    ) -> Self {
        Self { reached, resume }
    }
}

impl Input {
    pub fn new(bytes: Arc<[u8]>, pause: Option<Pause>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            pause,
        }
    }
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.bytes.position() >= (self.bytes.get_ref().len() / 2) as u64 {
            if let Some(pause) = self.pause.take() {
                pause.reached.send(()).expect("test is waiting for input");
                pause
                    .resume
                    .recv_timeout(Duration::from_secs(20))
                    .expect("test must release paused input");
            }
        }
        self.bytes.read(buffer)
    }
}
