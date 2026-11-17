//! `cfg(test)` only: the pool's in-memory duplex worker. Same serve loop and
//! framing as the subprocess, with the process boundary elided, so unit and
//! harness suites run unmodified.

use std::io;

use async_trait::async_trait;
use tokio::time::Duration;

use super::super::duplex as transport;
use super::super::proto::{self, AsyncFrameReader, FromWorker, Message, ToWorker};
use super::WorkerHandle;

pub struct DuplexWorker {
    sender: Option<flume::Sender<Vec<u8>>>,
    reader: AsyncFrameReader<transport::RxStream>,
}

pub async fn spawn() -> DuplexWorker {
    let pipes = transport::spawn_serve();
    let mut reader = AsyncFrameReader::new(transport::RxStream::new(pipes.from_child));
    let ready = tokio::time::timeout(Duration::from_secs(10), reader.read_frame())
        .await
        .expect("duplex serve thread alive")
        .expect("duplex pipe readable")
        .expect("ready frame");
    assert_eq!(
        proto::parse_frame::<FromWorker>(&ready).unwrap(),
        FromWorker::Ready
    );
    DuplexWorker {
        sender: Some(pipes.to_child),
        reader,
    }
}

#[async_trait]
impl WorkerHandle for DuplexWorker {
    async fn next_event(&mut self) -> io::Result<Message> {
        let body = self
            .reader
            .read_frame()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker closed"))?;
        let message = proto::parse_frame::<FromWorker>(&body)?.into_message()?;
        Ok(message)
    }

    async fn send(&mut self, frame: &ToWorker) -> io::Result<()> {
        let mut buffer = Vec::new();
        proto::encode_into(&mut buffer, frame)?;
        self.sender
            .as_ref()
            .expect("duplex sender kept until shutdown")
            .send(buffer)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker closed"))
    }

    async fn crash_detail(&mut self) -> String {
        "worker stopped without a result".to_owned()
    }

    async fn shutdown(&mut self) {
        // Dropping the parent half is the duplex EOF: the serve thread ends
        // the in-flight run as a cancellation and exits.
        self.sender = None;
    }

    fn pid(&self) -> Option<u32> {
        None
    }
}
