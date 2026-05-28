# Migration Implementation Plan

This document defines the phased execution plan for the smol→tokio and isahc→reqwest migration. Each phase is designed to be independently compilable and testable.

## Guiding Principles

1. **One crate per phase** where possible. Finish one, verify `cargo clippy + nextest`, move on.
2. **Leaf crates first.** Crates with no workspace-internal dependents get migrated first.
3. **Keep `flume` and `event-listener`.** Both are runtime-agnostic. Do not replace them.
4. **Keep `futures-lite` until all call sites are migrated.** Remove it in the final cleanup phase.
5. **Every phase ends with `cargo clippy --all-features --all --tests -- -D warnings` and `cargo nextest run --all-features --workspace` passing.**

## Dependency Order (leaves → root)

```
maki-storage       (leaf — no workspace deps)
maki-providers     (depends on maki-storage)
maki-agent         (depends on maki-providers, maki-config, maki-interpreter, maki-storage)
maki-lua           (depends on maki-agent, maki-config, maki-storage)
maki-ui            (depends on maki-agent, maki-providers, maki-config, maki-storage)
maki-docgen        (depends on maki-agent, maki-config, maki-providers, maki-ui)
root binary (src/) (depends on all above)
```

---

## Phase 0: Workspace Dependency Setup

**Goal**: Add tokio and reqwest to the workspace `Cargo.toml` without removing anything yet.

### Steps

1. Add to `[workspace.dependencies]` in root `Cargo.toml`:
   ```toml
    tokio = { version = "1", features = ["full"] }
    tokio-util = { version = "0.7", features = ["io", "compat"] }
    reqwest = { version = "0.13", default-features = false, features = [
      "charset", "http2", "macos-system-configuration", "stream"
    ] }
    futures = "0.3"
   ```

2. Do NOT remove `smol`, `isahc`, `futures-lite`, `async-process`, `async-lock`, `async-io` yet. Both runtimes will coexist during migration.

3. Run `cargo check --workspace` to verify the new deps resolve.

### Verification
- `cargo check --workspace` passes
- No code changes yet

---

## Phase 1: maki-storage (isahc → reqwest)

**Goal**: Remove `isahc` from `maki-storage`.

### Files to change
- `maki-storage/Cargo.toml` — add `reqwest` (workspace); remove `isahc`
- `maki-storage/src/version.rs` — rewrite HTTP calls

### Detailed changes

#### `maki-storage/Cargo.toml`
```toml
[dependencies]
# Remove: isahc = { workspace = true }
# Add:
reqwest = { workspace = true }
```

#### `maki-storage/src/version.rs`

The old code had two paths because `isahc` supports sync HTTP:
- `fetch_latest()` — synchronous
- `fetch_latest_async()` — async

With reqwest (async-only), there is only one async path. Callers that were synchronous now need an explicit runtime at the application layer.

**Translation**:

```rust
use std::time::Duration;
use reqwest::Client;
use serde_json;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/tontinton/maki/releases/latest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned HTTP {0}")]
    Status(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

fn client() -> Result<Client, VersionError> {
    Ok(Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

pub async fn fetch_latest() -> Result<String, VersionError> {
    let resp = client()?
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "maki")
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(VersionError::Status(status));
    }
    let bytes = resp.bytes().await?;
    parse_tag(&bytes)
}
```

Key differences:
- `reqwest::Client` replaces `isahc::HttpClient`
- No separate `Request` object construction — use builder chain on `client().get(url)`
- `resp.bytes()` returns `Bytes` (from `bytes` crate), works as `&[u8]` via `Deref`
- **Only one async function** — no sync wrapper. Callers that need sync must create their own runtime (see Phase 5 and Phase 7 notes below)

### Verification
- `cargo clippy -p maki-storage --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-storage`

---

## Phase 2: maki-providers (isahc → reqwest + smol → tokio)

**Goal**: Migrate the providers crate. This is the largest single crate change because all LLM providers use isahc for SSE streaming.

### Files to change
- `maki-providers/Cargo.toml`
- `maki-providers/src/providers/mod.rs`
- `maki-providers/src/providers/anthropic/mod.rs`
- `maki-providers/src/providers/anthropic/bedrock.rs`
- `maki-providers/src/providers/openai/auth.rs`
- `maki-providers/src/providers/openai/responses.rs`
- `maki-providers/src/providers/openai_compat.rs`
- `maki-providers/src/providers/google.rs`
- `maki-providers/src/providers/copilot/mod.rs`
- `maki-providers/src/providers/dynamic.rs`
- `maki-providers/src/provider.rs`

### `maki-providers/Cargo.toml`
```toml
[dependencies]
# Remove:
# smol = { workspace = true }
# isahc = { workspace = true }
# futures-lite = { workspace = true }
# Add:
tokio = { workspace = true }
reqwest = { workspace = true }
tokio-util = { workspace = true }
futures = { workspace = true }
```

### Core pattern: HTTP client factory

Replace `providers/mod.rs::http_client()`:

```rust
// Before (isahc)
pub(crate) fn http_client(timeouts: Timeouts) -> isahc::HttpClient {
    isahc::HttpClient::builder()
        .timeout(timeouts.request)
        .connect_timeout(timeouts.connect)
        .build()
        .expect("failed to create HTTP client")
}

// After (reqwest)
pub(crate) fn http_client(timeouts: Timeouts) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeouts.request)
        .connect_timeout(timeouts.connect)
        .build()
        .expect("failed to create HTTP client")
}
```

### Core pattern: SSE streaming

All providers stream SSE responses. The current pattern:

```rust
// Before (isahc + futures-lite)
pub(crate) async fn parse_sse(
    response: isahc::Response<isahc::AsyncBody>,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let reader = BufReader::new(response.into_body());
    let mut lines = reader.lines();
    // ... read lines via lines.next().await
}
```

Translation with reqwest + tokio-util:

```rust
// After (reqwest + tokio-util + futures)
use futures::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;

pub(crate) async fn parse_sse(
    response: reqwest::Response,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let stream = response.bytes_stream();
    let reader = StreamReader::new(stream.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
    let mut lines = futures::io::BufReader::new(reader).lines();
    // ... same line parsing logic
}
```

Note: Using `futures::io::BufReader` (not `tokio::io::BufReader`) because `StreamReader` implements `futures::io::AsyncRead`, not `tokio::io::AsyncRead`. The `lines()` method comes from `futures::io::AsyncBufReadExt`.

### Core pattern: `next_sse_line` timeout

Replace `smol::Timer` + `futures_lite::future::or`:

```rust
// Before
pub(crate) async fn next_sse_line<R: AsyncBufRead + Unpin>(
    lines: &mut futures_lite::io::Lines<R>,
    deadline: &mut Instant,
    stream_timeout: Duration,
) -> Result<Option<String>, AgentError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = futures_lite::future::or(
        async { lines.next().await.transpose().map_err(AgentError::from) },
        async {
            smol::Timer::after(remaining).await;
            Err(AgentError::Timeout { secs: stream_timeout.as_secs() })
        },
    ).await;
    // ...
}

// After
pub(crate) async fn next_sse_line<R: futures::io::AsyncBufRead + Unpin>(
    lines: &mut futures::io::Lines<R>,
    deadline: &mut Instant,
    stream_timeout: Duration,
) -> Result<Option<String>, AgentError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = tokio::select! {
        line = lines.next() => line.transpose().map_err(AgentError::from),
        _ = tokio::time::sleep(remaining) => Err(AgentError::Timeout { secs: stream_timeout.as_secs() }),
    };
    // ...
}
```

### Core pattern: Request building

```rust
// Before (isahc)
let request = Request::builder()
    .method("POST")
    .uri(url)
    .header("content-type", "application/json")
    .body(json_body)?;

// After (reqwest)
let request = client
    .post(url)
    .header("content-type", "application/json")
    .body(json_body)
    .build()?;
```

### Core pattern: `smol::unblock` → `tokio::task::spawn_blocking`

```rust
// Before
let provider = smol::unblock(move || kind.create(timeouts)).await?;

// After
let provider = tokio::task::spawn_blocking(move || kind.create(timeouts)).await??;
```

Note the double `?`: one for `JoinError` (if the task panics), one for the inner error.

### Core pattern: `smol::spawn` → `tokio::spawn`

```rust
// Before
smol::spawn(async move { /* ... */ }).detach();

// After
tokio::spawn(async move { /* ... */ });
```

`tokio::spawn` returns a `JoinHandle` that can be dropped (detached) or awaited.

### Core pattern: Mock response in tests

```rust
// Before
fn mock_response(data: &'static [u8]) -> isahc::Response<isahc::AsyncBody> {
    let body = isahc::AsyncBody::from_bytes_static(data);
    isahc::Response::builder().status(200).body(body).unwrap()
}

// After — use a mock reqwest Response via reqwest's test utilities
// or refactor parse_sse to accept an AsyncBufRead directly for testability
```

**Recommendation**: Refactor `parse_sse` and equivalent functions to accept `impl AsyncBufRead` instead of the concrete response type. This way tests can pass `Cursor<&[u8]>` directly without needing to construct mock HTTP responses. This is a test-only refactor but dramatically simplifies the migration.

### Test changes

Replace all `smol::block_on(async { ... })` in tests with `#[tokio::test]` annotation on the test function.

### File-by-file notes

**`provider.rs`**:
- `smol::unblock` → `tokio::task::spawn_blocking` (lines 248, 270)
- `smol::spawn` → `tokio::spawn` (line 275)

**`providers/anthropic/mod.rs`**:
- Type changes: `isahc::HttpClient` → `reqwest::Client`, `isahc::Request` → use builder chain
- `response.text().await` stays the same
- `response.into_body()` → `response.bytes_stream()` + `StreamReader`
- Mock response helper refactored

**`providers/anthropic/bedrock.rs`**:
- Uses `isahc::config::Configurable` for `timeout` on client builder → `reqwest::ClientBuilder::timeout`
- Uses `isahc::ReadResponseExt::bytes()` for sync reads → all async with reqwest

**`providers/openai/auth.rs`**:
- 4 functions that build `isahc::Request` objects and call `client.send(request)` synchronously
- These must become async or use `tokio::runtime::Runtime::block_on` for the sync path
- Recommend making the calling code async since the auth functions are called from async contexts

**`providers/openai/responses.rs`**:
- Same SSE streaming pattern as anthropic
- Uses `futures_lite::io::Cursor` in tests → `std::io::Cursor` with `futures::io::Cursor`

**`providers/openai_compat.rs`**:
- `build_request` returns `isahc::http::request::Builder` → return a closure or use reqwest builder
- `do_stream` method: same SSE pattern
- `do_list_models`: `response.text().await` stays similar

**`providers/google.rs`**:
- `build_request` returns `isahc::http::request::Builder` → use reqwest builder
- SSE streaming: same pattern
- `mock_response` helper: same refactoring

**`providers/copilot/mod.rs`**:
- Uses both anthropic SSE parser and openai_compat patterns
- `build_post` returns `isahc::http::request::Builder` → use reqwest builder
- `copilot_request` function signature changes

### Verification
- `cargo clippy -p maki-providers --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-providers`

---

## Phase 3: maki-agent (smol → tokio + isahc → reqwest)

**Goal**: Migrate the agent crate. This touches the MCP subsystem, tool implementations, and agent loop.

### Files to change
- `maki-agent/Cargo.toml`
- `maki-agent/src/task_set.rs`
- `maki-agent/src/cancel.rs`
- `maki-agent/src/child_guard.rs`
- `maki-agent/src/headless.rs`
- `maki-agent/src/mcp/stdio.rs`
- `maki-agent/src/mcp/http.rs`
- `maki-agent/src/mcp/mod.rs`
- `maki-agent/src/mcp/oauth/callback.rs`
- `maki-agent/src/mcp/oauth/discovery.rs`
- `maki-agent/src/mcp/oauth/token.rs`
- `maki-agent/src/mcp/oauth/registration.rs`
- `maki-agent/src/mcp/oauth/mod.rs`
- `maki-agent/src/agent/streaming.rs`
- `maki-agent/src/agent/run.rs`
- `maki-agent/src/agent/compaction.rs`
- `maki-agent/src/agent/tool_dispatch.rs`
- `maki-agent/src/tools/mod.rs`
- `maki-agent/src/tools/task.rs`
- `maki-agent/src/tools/code_execution.rs`

### `maki-agent/Cargo.toml`
```toml
[dependencies]
# Remove:
# smol = { workspace = true }
# async-process = { workspace = true }
# async-lock = { workspace = true }
# async-io = { workspace = true }
# futures-lite = { workspace = true }
# isahc = { workspace = true }
# Add:
tokio = { workspace = true }
tokio-util = { workspace = true }
reqwest = { workspace = true }
futures = { workspace = true }
```

**Note**: `futures-lite` must remain in `Cargo.toml` during Phase 3 because `agent/run.rs` uses `futures_lite::future::pending::<()>().await` in the compaction wait logic. It will be removed in Phase 8 cleanup.

### Key translations

#### `task_set.rs` — `smol::Task` → `tokio::task::JoinHandle`

```rust
// Before
pub struct TaskSet<T> {
    tasks: Vec<smol::Task<Result<T, String>>>,
}
impl<T: Send + 'static> TaskSet<T> {
    pub fn add(&mut self, fut: impl Future<Output = Result<T, String>> + Send + 'static) {
        self.tasks.push(smol::spawn(fut));
    }
    pub async fn join_all(self) -> Vec<Result<T, String>> {
        smol::block_on(async {
            // collect results
        })
    }
}

// After
pub struct TaskSet<T> {
    tasks: Vec<tokio::task::JoinHandle<Result<T, String>>>,
}
impl<T: Send + 'static> TaskSet<T> {
    pub fn add(&mut self, fut: impl Future<Output = Result<T, String>> + Send + 'static) {
        self.tasks.push(tokio::spawn(fut));
    }
    pub async fn join_all(self) -> Vec<Result<T, String>> {
        let mut results = Vec::with_capacity(self.tasks.len());
        for handle in self.tasks {
            match handle.await {
                Ok(result) => results.push(result),
                Err(_) => results.push(Err("task panicked".into())),
            }
        }
        results
    }
}
```

#### `child_guard.rs` — `async_process` → `tokio::process`

```rust
// Before
use async_process::Child;
// async_io::Timer::after(REAP_TIMEOUT).await;

// After
use tokio::process::Child;
// tokio::time::sleep(REAP_TIMEOUT).await;
```

`tokio::process::Child` has the same API surface: `.id()`, `.status().await`, `.kill()`.

#### `mcp/stdio.rs` — Process + I/O + channels

This is the most complex file. It uses:
- `async_process::Command` → `tokio::process::Command`
- `async_process::ChildStdin` → `tokio::process::ChildStdin`
- `async_process::Stdio` → `tokio::process::Stdio`
- `async_lock::Mutex` → `tokio::sync::Mutex`
- `smol::channel::bounded(1)` → `flume::bounded(1)` (for pending response map)
- `futures_lite::io::BufReader` → `tokio::io::BufReader` (or `futures::io::BufReader`)
- `futures_lite::AsyncBufReadExt::read_line` → `tokio::io::AsyncBufReadExt::read_line`
- `futures_lite::AsyncWriteExt::write_all` → `tokio::io::AsyncWriteExt::write_all`
- `async_io::Timer::after(timeout)` → `tokio::time::sleep(timeout)`

**Important**: `tokio::io::BufReader` and `futures::io::BufReader` are different types. Since `tokio::process::ChildStdout` implements `tokio::io::AsyncRead`, use `tokio::io::BufReader` here.

Similarly, `tokio::process::ChildStdin` implements `tokio::io::AsyncWrite`, so use `tokio::io::AsyncWriteExt`.

```rust
// Before
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use async_lock::Mutex;
use async_process::{ChildStdin, ...};
use smol::channel;

type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;

// After
use tokio::io::BufReader;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::process::{ChildStdin, Command, Stdio, Child};
use flume;

type PendingMap = HashMap<u64, flume::Sender<Result<Value, McpError>>>;
```

The `read_line` method on `tokio::io::BufReader` works the same way as the futures-lite version.

**Channel change detail**: `smol::channel::Sender::send(val).await` → `flume::Sender::send_async(val).await` (or `.send(val)` since flume's send is also sync).

Wait — check: in the current code, pending senders call `.send(result).await`. With `smol::channel::Sender`, `send` is async. With `flume::Sender`, `send` is synchronous (returns `Result<(), TrySendError>`). For async usage, use `send_async`. However, since the response is being sent once and the channel is bounded(1), synchronous `send` should work fine — if the receiver hasn't consumed yet, the channel is full. Use `try_send` or the blocking `send` (which blocks the current thread, not the async task). For async-correctness, use `send_async`.

#### `mcp/http.rs` — HTTP transport

Same pattern as maki-providers: `isahc::HttpClient` → `reqwest::Client`, `isahc::Request` → builder chain, `smol::unblock` → `tokio::task::spawn_blocking`.

#### `mcp/oauth/callback.rs` — TCP networking

```rust
// Before
use smol::net::TcpListener;
use smol::net::TcpStream;
use futures_lite::AsyncWriteExt;
use futures_lite::AsyncReadExt;

// After
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
```

TCP API is nearly identical between smol and tokio. The main difference is that tokio's `AsyncReadExt` and `AsyncWriteExt` come from `tokio::io` instead of `futures_lite::io`.

#### `cancel.rs` — Timer + spawn

```rust
// Before
smol::spawn(async move { ... }).detach();
smol::Timer::after(duration).await;

// After
tokio::spawn(async move { ... });
tokio::time::sleep(duration).await;
```

#### `agent/streaming.rs` and `agent/run.rs` — Future racing

```rust
// Before
futures_lite::future::race(async { ... }, async {
    smol::Timer::after(dur).await;
    Err(...)
}).await

// After — option A: tokio::select!
tokio::select! {
    result = async { ... } => result,
    _ = tokio::time::sleep(dur) => Err(...),
}

// After — option B: futures::future::select (if pinning needed)
let result = futures::future::select(
    Box::pin(async { ... }),
    Box::pin(async {
        tokio::time::sleep(dur).await;
        Err(...)
    }),
).await;
// Match on Either::Left / Either::Right
```

`tokio::select!` is preferred as it's more ergonomic and idiomatic.

#### `agent/compaction.rs` — `smol::block_on`

```rust
// Before
smol::block_on(async { ... })

// After
// This is called from a non-async context, so create a runtime
let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
rt.block_on(async { ... })
```

Or refactor the calling code to be async.

### Verification
- `cargo clippy -p maki-agent --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-agent`

---

## Phase 4: maki-lua (smol → tokio + isahc → reqwest)

**Goal**: Migrate the Lua plugin runtime. This is the most delicate phase because of `LocalExecutor`.

### Files to change
- `maki-lua/Cargo.toml`
- `maki-lua/src/runtime.rs`
- `maki-lua/src/api/net.rs`
- `maki-lua/src/api/fn_api.rs`
- `maki-lua/src/api/tool.rs`
- `maki-lua/src/api/async_api.rs`
- `maki-lua/src/api/fs.rs`
- `maki-lua/src/api/ui.rs`
- `maki-lua/tests/plugin_host.rs`

**Note**: `isahc` must be removed from `maki-lua/Cargo.toml` (it was present but not listed in original plan).

### `maki-lua/Cargo.toml`
```toml
[dependencies]
# Remove:
# smol = { workspace = true }
# futures-lite = { workspace = true }
# isahc = { workspace = true }
# Add:
tokio = { workspace = true }
tokio-util = { workspace = true }
reqwest = { workspace = true }
futures = { workspace = true }
```

### Critical: `runtime.rs` — LocalExecutor → LocalSet

The Lua runtime runs on a dedicated OS thread with a `smol::LocalExecutor` for `!Send` futures (Lua coroutines). The tokio equivalent is `tokio::task::LocalSet`.

```rust
// Before
let ex = Rc::new(smol::LocalExecutor::new());
smol::block_on(ex.run(async {
    loop {
        // process messages
    }
}));

// After
let local = tokio::task::LocalSet::new();
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .expect("failed to build runtime");
local.block_on(&rt, async {
    loop {
        // process messages
    }
});
```

Key differences:
- `LocalExecutor` is an smol concept; `LocalSet` is tokio's equivalent
- `LocalSet::block_on` requires a `&Runtime` handle
- `smol::LocalExecutor::spawn` → `tokio::task::spawn_local` (inside a `LocalSet`)
- `smol::future::yield_now()` → `tokio::task::yield_now()`

**The `Rc<smol::LocalExecutor>` pattern**: The current code passes `Rc<smol::LocalExecutor>` to `drain_spawn_queue` and other functions so they can spawn local tasks. With tokio, `spawn_local` doesn't need a reference to the LocalSet — it uses a thread-local. So the `ex` parameter can be removed from `drain_spawn_queue` and similar functions.

```rust
// Before
fn drain_spawn_queue(lua: &Lua, ex: &Rc<smol::LocalExecutor<'_>>, gate: &Rc<InflightGate>) {
    // ...
    ex.spawn(async move { /* local task */ });
}

// After
fn drain_spawn_queue(lua: &Lua, gate: &Rc<InflightGate>) {
    // ...
    tokio::task::spawn_local(async move { /* local task */ });
}
```

This only works if `drain_spawn_queue` is called inside a `LocalSet` context, which it is (it's called from within the `local.block_on` closure).

### `api/net.rs` — Lua networking plugin

The `net.request` function builds HTTP requests via isahc. Translate to reqwest:

```rust
// Before
use isahc::config::{Configurable, RedirectPolicy};
use isahc::{AsyncBody, HttpClient, Request};

fn build_request(url, user_agent, method, headers, body) -> Result<Request<AsyncBody>> {
    let mut builder = Request::builder().method(method).uri(url)
        .header("User-Agent", user_agent);
    // ...
    builder.body(AsyncBody::from(body))
}

async fn do_request(params) -> Result<ResponseData> {
    let client = HttpClient::builder()
        .timeout(params.timeout)
        .redirect_policy(RedirectPolicy::Limit(5))
        .build()?;
    let request = build_request(...)?;
    let mut response = client.send_async(request).await?;
    // ... read body with futures_lite::io::AsyncReadExt
}

// After
use reqwest::Client;

async fn do_request(params) -> Result<ResponseData> {
    let client = Client::builder()
        .timeout(params.timeout)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let mut builder = client.request(method, &params.url)
        .header("User-Agent", user_agent);
    for (k, v) in &params.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if !params.body.is_empty() {
        builder = builder.body(params.body.clone());
    }

    let response = builder.send().await?;
    let status = response.status().as_u16();
    let content_type = response.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Read body with size limit
    let body = response.bytes().await?;
    let body_str = String::from_utf8_lossy(&body[..body.len().min(params.max_bytes)]).into_owned();

    Ok(ResponseData { body: body_str, status, content_type })
}
```

### `api/fn_api.rs` — Future racing

Replace `futures_lite::pin!` + `futures_lite::future::or` with `tokio::select!`:

```rust
// Before
futures_lite::pin!(deadline);
let event = futures_lite::future::or(
    async { rx.recv_async().await.ok() },
    async { deadline.await; None },
).await;

// After
let event = tokio::select! {
    event = rx.recv_async() => event.ok(),
    _ = tokio::time::sleep_until(deadline) => None,
};
```

Note: `tokio::time::sleep_until` takes a `tokio::time::Instant` (different from `std::time::Instant`). Convert with `tokio::time::Instant::from_std(deadline)`.

### Test changes

All test functions using `smol::block_on(ex.run(async { ... }))` become:

```rust
#[tokio::test]
async fn test_name() {
    // or for local executor tests:
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
        // test body
    }).await;
}
```

### Verification
- `cargo clippy -p maki-lua --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-lua`

---

## Phase 5: maki-ui (smol → tokio)

**Goal**: Migrate the UI crate. No isahc usage here, only smol and async-process.

### Files to change
- `maki-ui/Cargo.toml`
- `maki-ui/src/event_loop.rs`
- `maki-ui/src/agent/agent_loop.rs`
- `maki-ui/src/agent/mod.rs`
- `maki-ui/src/agent/command_router.rs`
- `maki-ui/src/update.rs`
- `maki-ui/src/app/shell.rs`
- `maki-ui/src/app/btw.rs`

### `maki-ui/Cargo.toml`
```toml
[dependencies]
# Remove:
# smol = { workspace = true }
# futures-lite = { workspace = true }
# Add:
tokio = { workspace = true }
futures = { workspace = true }
```

### Key translations

#### `event_loop.rs` — Main event loop

The event loop currently uses `smol::Task` for background model fetching and `smol::spawn` for detaching tasks.

```rust
// Before
struct EventLoopApp {
    _model_fetch_task: smol::Task<()>,
    // ...
}

fn spawn_model_fetch() -> BackgroundModels {
    let task = smol::spawn(async move { ... });
    // ...
}

// After
struct EventLoopApp {
    _model_fetch_task: tokio::task::JoinHandle<()>,
    // ...
}

fn spawn_model_fetch() -> BackgroundModels {
    let task = tokio::spawn(async move { ... });
    // ...
}
```

#### `app/shell.rs` — Shell process spawning

Uses `async_process::{Command, Stdio}` and `futures_lite::io::{AsyncBufReadExt, BufReader}`:

```rust
// Before
use async_process::{Command, Stdio};
use futures_lite::io::{AsyncBufReadExt, BufReader};
use futures_lite::StreamExt;

// After
use tokio::process::{Command, Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::StreamExt; // or futures::StreamExt
```

The `spawn_line_reader` function returns a task that reads lines from a child process stdout. Translate:

```rust
// Before
fn spawn_line_reader<R: futures_lite::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    tx: flume::Sender<String>,
) -> smol::Task<()> {
    smol::spawn(async move {
        let mut reader = BufReader::new(reader);
        // ...
    })
}

// After
fn spawn_line_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    tx: flume::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        // ...
    })
}
```

#### `agent/mod.rs` — Shutdown with timeout

```rust
// Before
smol::block_on(async {
    let finished = futures_lite::future::or(
        async { task.await; true },
        async { smol::Timer::after(timeout).await; false },
    ).await;
    // ...
});

// After
let finished = tokio::select! {
    _ = &mut task => true,
    _ = tokio::time::sleep(timeout) => false,
};
```

Note: If `task` is a `JoinHandle`, you might need to handle the result. Also, the `smol::block_on` wrapper is no longer needed if the caller is already in a tokio context.

#### `update.rs` — Background version check

```rust
// Before
pub fn spawn_check() {
    smol::spawn(async { ... }).detach();
}

// After
pub fn spawn_check() {
    tokio::spawn(async { ... });
}
```

Note: `version::fetch_latest_async().await` becomes `version::fetch_latest().await` (the `_async` suffix is removed since there is only one async function now).

### Verification
- `cargo clippy -p maki-ui --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-ui`

---

## Phase 6: maki-docgen (smol → tokio)

**Goal**: Migrate the docgen tool.

### Files to change
- `maki-docgen/Cargo.toml`
- `maki-docgen/src/main.rs`

### Changes

```toml
# maki-docgen/Cargo.toml
[dependencies]
# Remove: smol = { workspace = true }
# Add:
tokio = { workspace = true }
futures = { workspace = true }
```

```rust
// Before
let ((tools, providers), (config, (keybindings, commands))) = smol::block_on(async {
    smol::future::zip(
        smol::future::zip(
            smol::unblock(gen_tools::generate),
            smol::unblock(gen_providers::generate),
        ),
        smol::future::zip(
            smol::unblock(gen_config::generate),
            smol::future::zip(
                smol::unblock(gen_keybindings::generate),
                smol::unblock(gen_commands::generate),
            ),
        ),
    ).await
});

// After
#[tokio::main]
async fn main() {
    let ((tools, providers), (config, (keybindings, commands))) = {
        let tools = tokio::task::spawn_blocking(gen_tools::generate);
        let providers = tokio::task::spawn_blocking(gen_providers::generate);
        let config = tokio::task::spawn_blocking(gen_config::generate);
        let keybindings = tokio::task::spawn_blocking(gen_keybindings::generate);
        let commands = tokio::task::spawn_blocking(gen_commands::generate);
        (
            (tools.await.unwrap(), providers.await.unwrap()),
            (config.await.unwrap(), (keybindings.await.unwrap(), commands.await.unwrap())),
        )
    };
    // ...
}
```

### Verification
- `cargo clippy -p maki-docgen --all-features --tests -- -D warnings`
- `cargo nextest run -p maki-docgen`
- `cargo run -p maki-docgen` (verify docs regenerate correctly)

---

## Phase 7: Root Binary Crate (smol → tokio + isahc → reqwest)

**Goal**: Migrate the main binary. Everything becomes async — no `block_on` wrappers.

### Files to change
- `Cargo.toml` (root, `[dependencies]`)
- `src/main.rs`
- `src/cmd/mod.rs`
- `src/cmd/subcmd.rs`
- `src/print.rs`
- `src/update.rs`

### `Cargo.toml` root `[dependencies]`

```toml
[dependencies]
# Remove:
# smol = { workspace = true }
# futures-lite = { workspace = true }
# isahc = { workspace = true }
# Add:
tokio = { workspace = true }
reqwest = { workspace = true }
futures = { workspace = true }
```

### `src/main.rs` — Async entrypoint

```rust
// Before
fn main() {
    color_eyre::install().ok();
    if let Err(e) = cmd::dispatch(Cli::parse()) {
        print_error(&e);
        std::process::exit(1);
    }
}

// After
#[tokio::main]
async fn main() {
    color_eyre::install().ok();
    if let Err(e) = cmd::dispatch(Cli::parse()).await {
        print_error(&e);
        std::process::exit(1);
    }
}
```

### `src/cmd/mod.rs` — Async dispatch

```rust
// Before
pub fn dispatch(cli: Cli) -> Result<()> { ... }

// After
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        // ...
        Some(Command::Models) => subcmd::models().await,
        Some(Command::Index { path }) => subcmd::index(&path, cli.no_plugins).await?,
        Some(Command::Update { yes, no_color }) => update::update(yes, no_color).await?,
        Some(Command::Rollback) => update::rollback().await?,
        // ...
    }
}
```

### `src/cmd/subcmd.rs` — Async subcommands

Every function that currently wraps async code in `smol::block_on` becomes `async fn`:

```rust
// Before
pub fn models() {
    smol::block_on(fetch_all_models(|batch| { ... }));
}

pub fn index(path: &str, no_plugins: bool) -> Result<()> {
    // ...
    let result = smol::block_on(async { inv.execute(&ctx).await });
    // ...
}

pub fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    smol::block_on(async { ... })
}

// After
pub async fn models() {
    fetch_all_models(|batch| { ... }).await;
}

pub async fn index(path: &str, no_plugins: bool) -> Result<()> {
    // ...
    let result = inv.execute(&ctx).await;
    // ...
}

pub async fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    // ... body is already async, just remove the block_on wrapper
}
```

### `src/update.rs` — Async update + rollback

Both functions become async. `fetch_script()` also becomes async since `reqwest` is async-only.

```rust
// Before
fn fetch_script() -> Result<String, UpdateError> {
    use isahc::ReadResponseExt;
    let mut response = isahc::get(INSTALL_SCRIPT_URL)?;
    response.text().map_err(...)
}

pub fn update(skip_confirm: bool, no_color: bool) -> Result<(), UpdateError> {
    let latest = version::fetch_latest()?;
    // ...
    let script = fetch_script()?;
    // ...
}

pub fn rollback() -> Result<(), UpdateError> { ... }

// After
async fn fetch_script() -> Result<String, UpdateError> {
    reqwest::get(INSTALL_SCRIPT_URL)
        .await?
        .text()
        .await
        .map_err(|e| UpdateError::Fetch { url: INSTALL_SCRIPT_URL, source: e })
}

pub async fn update(skip_confirm: bool, no_color: bool) -> Result<(), UpdateError> {
    let latest = version::fetch_latest().await?;
    // ...
    let script = fetch_script().await?;
    // ...
}

pub async fn rollback() -> Result<(), UpdateError> { ... }
```

Note: `UpdateError::Fetch` changes from `source: isahc::Error` to `source: reqwest::Error`.

### `src/print.rs` — Headless mode

```rust
// Before
let (mcp_handle, mcp_config_errors) = smol::block_on(maki_agent::mcp::start(&cwd));
while let Ok(envelope) = smol::block_on(event_rx.recv_async()) { ... }
smol::block_on(async {
    futures_lite::future::or(task, async {
        smol::Timer::after(AGENT_SHUTDOWN_TIMEOUT).await;
    }).await;
});

// After — already in async context, no block_on needed
let (mcp_handle, mcp_config_errors) = maki_agent::mcp::start(&cwd).await;
while let Ok(envelope) = event_rx.recv_async().await { ... }
tokio::select! {
    _ = task => {},
    _ = tokio::time::sleep(AGENT_SHUTDOWN_TIMEOUT) => {},
}
```

### Verification
- `cargo clippy --all-features --all --tests -- -D warnings`
- `cargo nextest run --all-features --workspace`
- Manual smoke test: `cargo run -- --help`

---

## Phase 8: Cleanup — Remove smol Ecosystem

**Goal**: Remove all smol-family dependencies from `Cargo.toml` workspace.

### Steps

1. Remove from `[workspace.dependencies]`:
   ```toml
   # Remove these:
   smol = "2"
   async-process = "2"
   async-lock = "3"
   async-io = "2"
   futures-lite = "2"
   isahc = { version = "1.7", ... }
   ```

2. Verify no crate still references these via `Cargo.toml`.

3. Run `cargo check --workspace` to confirm everything compiles.

4. Run `cargo clippy --all-features --all --tests -- -D warnings`.

5. Run `cargo nextest run --all-features --workspace`.

6. Run `cargo run -p maki-docgen` to verify doc generation still works.

7. Run `cargo build --release` to verify release build.

### Verification
- Full workspace builds and tests pass
- `cargo tree -i smol` returns nothing
- `cargo tree -i isahc` returns nothing
- `cargo tree -i futures-lite` returns nothing

---

## Testing Strategy

### Per-phase

Each phase must pass before moving to the next:
1. `cargo clippy -p <crate> --all-features --tests -- -D warnings` — zero warnings
2. `cargo nextest run -p <crate>` — all tests pass
3. Manual spot-check of changed files

### End-to-end

After Phase 8:
1. `cargo clippy --all-features --all --tests -- -D warnings`
2. `cargo nextest run --all-features --workspace`
3. `cargo build --release`
4. `cargo run -- -p "hello, respond briefly"` — smoke test with an LLM provider
5. `cargo run -p maki-docgen` — doc generation

### Regression checklist

- [ ] Anthropic streaming works (SSE)
- [ ] OpenAI/compatible streaming works (SSE)
- [ ] Google Gemini streaming works (SSE)
- [ ] Copilot streaming works
- [ ] Bedrock streaming works
- [ ] MCP stdio transport works (spawns child processes)
- [ ] MCP HTTP transport works
- [ ] MCP OAuth flow works (TCP listener for callback)
- [ ] Version check works
- [ ] Self-update works
- [ ] Lua plugin HTTP requests work
- [ ] Lua plugin async tool calls work
- [ ] Shell command execution in UI works
- [ ] Agent shutdown with timeout works
- [ ] Cancel token propagation works
- [ ] Background model fetch works
