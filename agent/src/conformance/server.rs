//! The conformance mock HTTP server: a hand-rolled tokio `TcpListener`
//! (mirroring animist's own provider tests — no mock crate) that serves a
//! scripted queue of replies, records every request, and writes bodies in
//! small chunks so SSE parsers see fragmented frames.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};

/// One scripted HTTP reply.
#[derive(Clone)]
pub(super) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// A JSON reply with the given status.
    pub(super) fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).expect("reply body serializes"),
        }
    }

    /// An SSE reply carrying `body` verbatim.
    pub(super) fn sse(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }
}

/// One recorded request: the parts the conformance assertions read.
#[derive(Clone, Debug)]
pub(super) struct Recorded {
    pub method: String,
    pub path: String,
    /// The `authorization` header, when present.
    pub authorization: Option<String>,
    /// The `x-api-key` header, when present.
    pub x_api_key: Option<String>,
    /// The parsed JSON body (`Value::Null` when the body is not JSON).
    pub body: Value,
}

impl Recorded {
    /// The request body's JSON text (empty when the body was not JSON).
    pub(super) fn text(&self) -> String {
        match &self.body {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}

/// A local mock server bound to `127.0.0.1:0`.
pub(super) struct MockServer {
    addr: SocketAddr,
    replies: Arc<Mutex<Vec<Reply>>>,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serve `replies` in order, one per request; a request with no queued
    /// reply gets a 500 so a missing script fails loudly.
    pub(super) async fn spawn(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local addr");
        let replies = Arc::new(Mutex::new(replies));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let server_replies = Arc::clone(&replies);
        let server_recorded = Arc::clone(&recorded);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (mut reader, mut writer) = tokio::io::split(stream);
                let reply =
                    serve_request(&mut reader, &mut writer, &server_replies, &server_recorded)
                        .await;
                if let Err(e) = reply {
                    eprintln!("conformance mock server request failed: {e}");
                }
            }
        });
        Self {
            addr,
            replies,
            recorded,
        }
    }

    /// The bound port, for BML base URLs.
    pub(super) fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The requests served so far, in order.
    pub(super) fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("recorded lock").clone()
    }

    pub(super) fn request_count(&self) -> usize {
        self.recorded.lock().expect("recorded lock").len()
    }

    /// How many replies remain queued (for retry pacing assertions).
    pub(super) fn pending(&self) -> usize {
        self.replies.lock().expect("replies lock").len()
    }
}

/// Read one request, record it, and serve the next queued reply.
async fn serve_request(
    reader: &mut ReadHalf<TcpStream>,
    writer: &mut WriteHalf<TcpStream>,
    replies: &Mutex<Vec<Reply>>,
    recorded: &Mutex<Vec<Recorded>>,
) -> std::io::Result<()> {
    // A client that connects and stalls must not wedge the accept loop
    // (and, through it, every later connection).
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(reader))
        .await
        .map_err(|_| std::io::Error::other("conformance mock: head read timed out"))??;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0usize;
    let mut authorization = None;
    let mut x_api_key = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "authorization" => authorization = Some(value.to_owned()),
            "x-api-key" => x_api_key = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        tokio::time::timeout(Duration::from_secs(10), reader.read_exact(&mut body))
            .await
            .map_err(|_| std::io::Error::other("conformance mock: body read timed out"))??;
    }
    let body_json = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    recorded.lock().expect("recorded lock").push(Recorded {
        method,
        path,
        authorization,
        x_api_key,
        body: body_json,
    });

    // One critical section: dequeue-or-fallback stays atomic so a future
    // per-connection spawn cannot serve one reply twice.
    let reply = {
        let mut queue = replies.lock().expect("replies lock");
        if queue.is_empty() {
            Reply::json(
                500,
                serde_json::json!({"error": "conformance mock ran out of scripted replies"}),
            )
        } else {
            queue.remove(0)
        }
    };
    write_reply(writer, &reply).await
}

/// Read bytes until the end of the request head.
async fn read_head(reader: &mut ReadHalf<TcpStream>) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        reader.read_exact(&mut byte).await?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

async fn write_reply(writer: &mut WriteHalf<TcpStream>, reply: &Reply) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reply.reason());
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\n", reply.body.len()));
    head.push_str("connection: close\r\n\r\n");
    writer.write_all(head.as_bytes()).await?;
    // Chunked writes with yields: the providers' SSE and eventstream
    // parsers must reassemble frames split across reads.
    for chunk in reply.body.chunks(64) {
        writer.write_all(chunk).await?;
        writer.flush().await?;
        tokio::task::yield_now().await;
    }
    writer.shutdown().await
}
