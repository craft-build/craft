//! The codemode worker wire protocol: `u32` LE length-prefixed JSON frames
//! over stdio (the `crafty __codemode-worker` subprocess) or an in-memory
//! duplex (tests). One protocol, two transports — the child-side serve loop
//! and every parent-side handle go through this module.
//!
//! Frames are one JSON object each:
//!
//! - child → parent: `{"type":"ready"}`, `{"type":"print","text":…}`,
//!   `{"type":"calls","calls":[PendingCall]}`,
//!   `{"type":"done","stdout":…,"result":{"Ok":…}|{"Err":Failure}}`
//! - parent → child:
//!   `{"type":"run","code":…,"timeout_secs":…,"max_output_kib":…,"max_tool_calls":…,"names":[…]}`,
//!   `{"type":"replies","replies":[[id,{"ok":…}|{"err":…}]]}`

use std::collections::BTreeMap;
use std::io;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::diagnose::Failure;

/// Decode cap for one frame. Tool replies can legitimately reach several MB
/// (`read_text`); the child's own output stays capped by its output limit.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

const PREFIX_BYTES: usize = 4;

/// A tool call suspended inside the interpreter, marshalled to the parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub id: u32,
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: BTreeMap<String, Value>,
}

/// One run's final state from the interpreter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finished {
    pub stdout: String,
    pub result: Result<Option<String>, Failure>,
}

/// Events from the interpreter loop to the parent, translated to and from
/// [`FromWorker`] frames at the transport edge.
#[derive(Debug, Clone)]
pub enum Message {
    Calls(Vec<PendingCall>),
    Print(String),
    Done(Finished),
}

/// A tool reply returned into the interpreter, translated to and from
/// [`WireReply`] at the transport edge.
pub type Reply = (u32, Result<Value, String>);

/// Per-run parameters. `CodemodeConfig` itself is never serialized; a pool
/// worker stays stateless between runs because everything travels here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerParams {
    pub code: String,
    pub timeout_secs: u64,
    pub max_output_kib: usize,
    pub max_tool_calls: usize,
    pub names: Vec<String>,
}

/// One tool reply on the wire: `[id, {"ok": value} | {"err": message}]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyOutcome {
    Ok(Value),
    Err(String),
}

pub type WireReply = (u32, ReplyOutcome);

/// Parent → child frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToWorker {
    Run(WorkerParams),
    Replies { replies: Vec<WireReply> },
}

/// Child → parent frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FromWorker {
    Ready,
    Print {
        text: String,
    },
    Calls {
        calls: Vec<PendingCall>,
    },
    Done {
        stdout: String,
        result: Result<Option<String>, Failure>,
    },
}

impl From<Message> for FromWorker {
    fn from(message: Message) -> Self {
        match message {
            Message::Calls(calls) => Self::Calls { calls },
            Message::Print(text) => Self::Print { text },
            Message::Done(Finished { stdout, result }) => Self::Done { stdout, result },
        }
    }
}

impl FromWorker {
    /// The bridge-side event; `Ready` is consumed at handshake and is a
    /// protocol error afterwards.
    pub fn into_message(self) -> io::Result<Message> {
        match self {
            Self::Ready => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected ready frame",
            )),
            Self::Print { text } => Ok(Message::Print(text)),
            Self::Calls { calls } => Ok(Message::Calls(calls)),
            Self::Done { stdout, result } => Ok(Message::Done(Finished { stdout, result })),
        }
    }
}

impl ReplyOutcome {
    pub fn from_result(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(err) => Self::Err(err),
        }
    }

    pub fn into_result(self) -> Result<Value, String> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(err) => Err(err),
        }
    }
}

/// Appends one length-prefixed frame to `buffer`; the caller writes the
/// buffer out and flushes.
pub fn encode_into(buffer: &mut Vec<u8>, frame: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32 length"))?;
    buffer.reserve(PREFIX_BYTES + body.len());
    buffer.extend_from_slice(&len.to_le_bytes());
    buffer.extend_from_slice(&body);
    Ok(())
}

/// Parses a frame body.
pub fn parse_frame<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Blocking frame read; `Ok(None)` on clean EOF at a frame boundary.
pub fn read_frame(reader: &mut impl io::Read) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; PREFIX_BYTES];
    let mut filled = 0;
    while filled < PREFIX_BYTES {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame prefix",
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    let len = frame_len(prefix)?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|error| io::Error::new(io::ErrorKind::UnexpectedEof, error))?;
    Ok(Some(body))
}

fn frame_len(prefix: [u8; PREFIX_BYTES]) -> io::Result<usize> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_FRAME_BYTES} cap"),
        ));
    }
    Ok(len)
}

/// Cancellation-safe async frame reader: every partial read lands in
/// `buffer`, so a dropped `read_frame` future resumes cleanly.
pub struct AsyncFrameReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> AsyncFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
        }
    }

    /// Reads one frame body; `Ok(None)` on clean EOF at a frame boundary.
    pub async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(Some(frame));
            }
            let mut chunk = [0u8; 8192];
            let read = tokio::io::AsyncReadExt::read(&mut self.reader, &mut chunk).await?;
            if read == 0 {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated frame",
                    ))
                };
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn take_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buffer.len() < PREFIX_BYTES {
            return Ok(None);
        }
        let prefix: [u8; PREFIX_BYTES] = self.buffer[..PREFIX_BYTES].try_into().unwrap();
        let len = frame_len(prefix)?;
        if self.buffer.len() < PREFIX_BYTES + len {
            return Ok(None);
        }
        let body: Vec<u8> = self
            .buffer
            .drain(..PREFIX_BYTES + len)
            .skip(PREFIX_BYTES)
            .collect();
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(frame: &T) -> T
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let mut buffer = Vec::new();
        encode_into(&mut buffer, frame).unwrap();
        let mut cursor = io::Cursor::new(buffer);
        let body = read_frame(&mut cursor).unwrap().expect("one frame");
        assert!(read_frame(&mut cursor).unwrap().is_none(), "nothing left");
        parse_frame(&body).unwrap()
    }

    #[test]
    fn frame_codec_round_trips_every_child_frame() {
        assert_eq!(
            round_trip(&FromWorker::Ready),
            FromWorker::Ready,
            "ready frame survives the codec"
        );
        let done = FromWorker::Done {
            stdout: "out".to_owned(),
            result: Ok(Some("value".to_owned())),
        };
        assert_eq!(round_trip(&done), done);
        let failed = FromWorker::Done {
            stdout: String::new(),
            result: Err(Failure::crash("engine died")),
        };
        assert_eq!(round_trip(&failed), failed);
        let calls = FromWorker::Calls {
            calls: vec![PendingCall {
                id: 7,
                name: "read_text".to_owned(),
                args: vec![Value::String("a.txt".to_owned())],
                kwargs: BTreeMap::from([("path".to_owned(), Value::Bool(true))]),
            }],
        };
        assert_eq!(round_trip(&calls), calls);
    }

    #[test]
    fn wire_shapes_match_the_protocol() {
        assert_eq!(
            serde_json::to_value(FromWorker::Ready).unwrap(),
            serde_json::json!({"type": "ready"})
        );
        assert_eq!(
            serde_json::to_value(&ToWorker::Replies {
                replies: vec![(3, ReplyOutcome::Ok(Value::Null))]
            })
            .unwrap(),
            serde_json::json!({"type": "replies", "replies": [[3, {"ok": null}]]})
        );
        assert_eq!(
            serde_json::to_value(&(9, ReplyOutcome::Err("missing".to_owned()))).unwrap(),
            serde_json::json!([9, {"err": "missing"}])
        );
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
        let error = read_frame(&mut io::Cursor::new(buffer)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn failure_and_class_survive_serde() {
        let failure = Failure {
            class: super::super::diagnose::Class::Crash,
            line: Some(12),
            message: "boom".to_owned(),
            detail: "boom (detail)".to_owned(),
            hint: Some("retry".to_owned()),
        };
        let done = FromWorker::Done {
            stdout: String::new(),
            result: Err(failure.clone()),
        };
        let decoded: FromWorker = round_trip(&done);
        let FromWorker::Done {
            result: Err(failure_out),
            ..
        } = decoded
        else {
            panic!("decoded as the failure variant");
        };
        assert_eq!(failure_out, failure);
    }

    #[tokio::test]
    async fn async_reader_round_trips_split_chunks() {
        let mut buffer = Vec::new();
        encode_into(&mut buffer, &FromWorker::Ready).unwrap();
        encode_into(
            &mut buffer,
            &FromWorker::Print {
                text: "line".to_owned(),
            },
        )
        .unwrap();
        let mut source = &buffer[..];
        let mut reader = AsyncFrameReader::new(&mut source);
        let first = reader.read_frame().await.unwrap().expect("ready");
        assert_eq!(
            parse_frame::<FromWorker>(&first).unwrap(),
            FromWorker::Ready
        );
        let second = reader.read_frame().await.unwrap().expect("print");
        assert!(matches!(
            parse_frame::<FromWorker>(&second).unwrap(),
            FromWorker::Print { .. }
        ));
        assert!(reader.read_frame().await.unwrap().is_none(), "clean EOF");
    }
}
