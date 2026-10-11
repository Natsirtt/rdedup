//! Response frames decouple bounded storage production from HTTP backpressure.
use axum::body::Bytes;
use futures_util::Stream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

pub(crate) enum BodyPiece {
    Bytes(Bytes),
    Failed(io::Error),
}

pub(crate) struct ResponseStream {
    receiver: mpsc::Receiver<BodyPiece>,
}

impl Stream for ResponseStream {
    type Item = io::Result<Bytes>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context).map(|piece| {
            piece.map(|piece| match piece {
                BodyPiece::Bytes(bytes) => Ok(bytes),
                BodyPiece::Failed(error) => Err(error),
            })
        })
    }
}

impl ResponseStream {
    pub(crate) fn new(receiver: mpsc::Receiver<BodyPiece>) -> Self {
        Self { receiver }
    }
}
