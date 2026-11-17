//! In-memory duplex transport for tests: the same serve loop and framing as
//! the subprocess, with only the process boundary elided. Unit and harness
//! suites exercise the real protocol through this; the pool falls back to it
//! under `cfg(test)` when no worker command override is set.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;

use tokio::sync::mpsc::UnboundedReceiver;

use super::worker;

/// The parent side of one duplex worker: frames written to `to_child`,
/// byte chunks read back from `from_child`.
pub struct DuplexPipes {
    pub to_child: flume::Sender<Vec<u8>>,
    pub from_child: UnboundedReceiver<Vec<u8>>,
}

/// Spawns the real serve loop over a fresh in-memory pipe pair.
pub fn spawn_serve() -> DuplexPipes {
    let (to_child_tx, to_child_rx) = flume::unbounded::<Vec<u8>>();
    let (to_parent_tx, to_parent_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    thread::Builder::new()
        .name("craft-codemode-duplex".to_owned())
        .spawn(move || {
            worker::serve(
                ChannelReader {
                    receiver: to_child_rx,
                    pending: VecDeque::new(),
                },
                ChannelWriter {
                    sender: to_parent_tx,
                },
            );
        })
        .expect("spawn duplex serve thread");
    DuplexPipes {
        to_child: to_child_tx,
        from_child: to_parent_rx,
    }
}

/// Child-side blocking read half over a chunk channel; EOF when the parent
/// side is gone.
pub struct ChannelReader {
    receiver: flume::Receiver<Vec<u8>>,
    pending: VecDeque<u8>,
}

impl io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.receiver.recv() {
                Ok(chunk) => self.pending.extend(chunk),
                Err(_) => return Ok(0),
            }
        }
        let count = buf.len().min(self.pending.len());
        for (slot, byte) in buf.iter_mut().zip(self.pending.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }
}

/// Child-side write half: each write becomes one chunk (the serve loop is
/// a sync thread, so `blocking_send` is correct here).
struct ChannelWriter {
    sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sender
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "duplex closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Parent-side async read half: a chunk channel as `tokio::io::AsyncRead`.
/// `poll_recv` caches its waker in the receiver, so re-created read futures
/// stay live.
pub struct RxStream {
    receiver: UnboundedReceiver<Vec<u8>>,
    pending: VecDeque<u8>,
}

impl RxStream {
    pub fn new(receiver: UnboundedReceiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            pending: VecDeque::new(),
        }
    }
}

impl tokio::io::AsyncRead for RxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pending.is_empty() {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => this.pending.extend(chunk),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let count = buf.remaining().min(this.pending.len());
        let chunk: Vec<u8> = this.pending.drain(..count).collect();
        buf.put_slice(&chunk);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto;
    use super::*;

    async fn read_frame(
        reader: &mut proto::AsyncFrameReader<RxStream>,
    ) -> Option<proto::FromWorker> {
        let body = reader.read_frame().await.unwrap()?;
        Some(proto::parse_frame(&body).unwrap())
    }

    fn send_frame(to_child: &flume::Sender<Vec<u8>>, frame: &impl serde::Serialize) {
        let mut buffer = Vec::new();
        proto::encode_into(&mut buffer, frame).unwrap();
        to_child.send(buffer).unwrap();
    }

    fn trivial_params() -> proto::WorkerParams {
        proto::WorkerParams {
            code: String::new(),
            timeout_secs: 5,
            max_output_kib: 32,
            max_tool_calls: 8,
            names: Vec::new(),
        }
    }

    #[tokio::test]
    async fn serve_handshakes_ready_and_runs_scripts() {
        let DuplexPipes {
            to_child,
            from_child,
        } = spawn_serve();
        let mut reader = proto::AsyncFrameReader::new(RxStream::new(from_child));
        assert!(matches!(
            read_frame(&mut reader).await,
            Some(proto::FromWorker::Ready)
        ));
        send_frame(&to_child, &proto::ToWorker::Run(trivial_params()));
        match read_frame(&mut reader).await {
            Some(proto::FromWorker::Done { result, .. }) => {
                assert_eq!(result, Ok(None), "an empty script completes as None");
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serve_recycles_after_the_run_cap() {
        let DuplexPipes {
            to_child,
            from_child,
        } = spawn_serve();
        let mut reader = proto::AsyncFrameReader::new(RxStream::new(from_child));
        assert!(matches!(
            read_frame(&mut reader).await,
            Some(proto::FromWorker::Ready)
        ));
        for _ in 0..worker::RECYCLE_AFTER_RUNS {
            send_frame(&to_child, &proto::ToWorker::Run(trivial_params()));
            assert!(
                matches!(
                    read_frame(&mut reader).await,
                    Some(proto::FromWorker::Done { .. })
                ),
                "every run completes"
            );
        }
        // The next run never starts: the worker self-exited, so the pipe
        // hits clean EOF.
        send_frame(&to_child, &proto::ToWorker::Run(trivial_params()));
        assert!(
            reader.read_frame().await.unwrap().is_none(),
            "worker exits after the recycle cap"
        );
    }

    #[tokio::test]
    async fn closing_the_parent_side_stops_the_worker() {
        // A script blocked on a tool reply parks the interpreter; dropping
        // the parent side signals EOF, which ends the run as a cancellation
        // and exits the serve loop.
        let (to_child_tx, to_child_rx) = flume::unbounded::<Vec<u8>>();
        let (to_parent_tx, to_parent_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let serve = thread::Builder::new()
            .name("craft-codemode-duplex".to_owned())
            .spawn(move || {
                worker::serve(
                    ChannelReader {
                        receiver: to_child_rx,
                        pending: VecDeque::new(),
                    },
                    ChannelWriter {
                        sender: to_parent_tx,
                    },
                );
            })
            .unwrap();
        let DuplexPipes {
            to_child,
            from_child,
        } = DuplexPipes {
            to_child: to_child_tx,
            from_child: to_parent_rx,
        };
        let mut reader = proto::AsyncFrameReader::new(RxStream::new(from_child));
        assert!(matches!(
            read_frame(&mut reader).await,
            Some(proto::FromWorker::Ready)
        ));
        send_frame(
            &to_child,
            &proto::ToWorker::Run(proto::WorkerParams {
                code: "result = await read_text('a.txt')".to_owned(),
                names: vec!["read_text".to_owned()],
                ..trivial_params()
            }),
        );
        assert!(matches!(
            read_frame(&mut reader).await,
            Some(proto::FromWorker::Calls { .. }),
        ));
        drop(to_child);
        drop(reader);
        let _ = serve.join();
    }
}
