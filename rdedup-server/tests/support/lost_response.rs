//! A TCP fault at the acknowledgement boundary of a committed object write.
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    CommittedWithoutResponse,
    RetriedWrite,
}
pub(super) struct Proxy {
    pub(super) address: String,
    pub(super) events: mpsc::UnboundedReceiver<Event>,
    task: JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Proxy {
    pub(super) async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (sender, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut interrupted_body = None;
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut bytes = [0; 1024];
                    let length = client.read(&mut bytes).await.unwrap();
                    assert_ne!(length, 0);
                    request.extend_from_slice(&bytes[..length]);
                    if let Some(end) = request
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                    {
                        break end + 4;
                    }
                    assert!(request.len() <= 64 * 1024);
                };
                let headers =
                    String::from_utf8(request[..header_end].to_vec()).unwrap();
                let content_length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| {
                        name.eq_ignore_ascii_case("content-length")
                    })
                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                assert!(content_length <= 1024 * 1024);
                while request.len() < header_end + content_length {
                    let mut bytes = [0; 1024];
                    let length = client.read(&mut bytes).await.unwrap();
                    assert_ne!(length, 0);
                    request.extend_from_slice(&bytes[..length]);
                }
                let is_write = headers.starts_with("PUT /api/v1/objects/");
                let mut forwarded = headers
                    .lines()
                    .filter(|line| {
                        !line.is_empty()
                            && !line
                                .to_ascii_lowercase()
                                .starts_with("connection:")
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n")
                    .into_bytes();
                forwarded.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
                forwarded.extend_from_slice(&request[header_end..]);
                let mut server = TcpStream::connect(&upstream).await.unwrap();
                server.write_all(&forwarded).await.unwrap();
                let mut response = Vec::new();
                server.read_to_end(&mut response).await.unwrap();
                if is_write {
                    match &interrupted_body {
                        None => {
                            assert!(response.starts_with(b"HTTP/1.1 204"));
                            interrupted_body =
                                Some(request[header_end..].to_vec());
                            sender
                                .send(Event::CommittedWithoutResponse)
                                .unwrap();
                            // The server has committed; the client never sees its acknowledgement.
                            drop(client);
                            continue;
                        }
                        Some(original) => {
                            assert_eq!(original, &request[header_end..]);
                            sender.send(Event::RetriedWrite).unwrap();
                        }
                    }
                }
                client.write_all(&response).await.unwrap();
            }
        });
        Self {
            address,
            events,
            task,
        }
    }
}
