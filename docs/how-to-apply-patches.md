# How to Apply Patches: smol to tokio / isahc to reqwest

This document is a guide for AI agents porting patches from the old smol/isahc codebase to the new tokio/reqwest system. Read this before editing any code.

## Context

The maki codebase is migrating from:
- **smol** async runtime to **tokio**
- **isahc** HTTP client to **reqwest**
- **futures-lite** / **async-process** / **async-lock** / **async-io** to tokio equivalents

Some crates may already be migrated. Others may still use the old stack. When applying a patch from an old revision, you must translate the old APIs to the new ones.

## Step by Step

### 1. Check if the target file is already migrated

Before applying any patch, read the current version of the file. If it already uses tokio/reqwest, skip the translation and apply the patch logic directly.

If the file still uses smol/isahc, proceed to translate.

### 2. Identify which old APIs the patch uses

Look for these patterns in the patch:

**Runtime:**
- `smol::block_on(...)`
- `smol::spawn(...)` / `smol::spawn(...).detach()`
- `smol::unblock(...)`
- `smol::LocalExecutor::new()` / `ex.run(...)`
- `smol::Timer::after(dur)` / `smol::Timer::at(instant)`
- `smol::Task<T>`
- `smol::net::TcpListener` / `smol::net::TcpStream`
- `smol::future::yield_now()`
- `smol::channel::bounded(n)` / `smol::channel::unbounded()`

**HTTP (isahc):**
- `isahc::HttpClient`
- `isahc::Request::builder()`
- `isahc::Response<isahc::AsyncBody>`
- `response.into_body()`
- `isahc::config::Configurable`
- `isahc::ReadResponseExt::bytes()` / `isahc::AsyncReadResponseExt::bytes()`
- `client.send(request)` (sync)
- `client.send_async(request).await`

**I/O and Async Utilities:**
- `futures_lite::io::BufReader`
- `futures_lite::io::AsyncBufReadExt`
- `futures_lite::io::AsyncWriteExt`
- `futures_lite::future::race(a, b)`
- `futures_lite::future::or(a, b)`
- `futures_lite::StreamExt`
- `futures_lite::pin!`
- `async_process::Command` / `Child` / `ChildStdin` / `Stdio`
- `async_lock::Mutex`
- `async_io::Timer::after(dur)`

### 3. Apply the translation

Use this translation table:

| Old | New |
|---|---|
| `smol::block_on(fut)` | `tokio::runtime::Runtime::new().unwrap().block_on(fut)` or `#[tokio::main]` / `#[tokio::test]` |
| `smol::spawn(fut)` | `tokio::spawn(fut)` |
| `smol::spawn(fut).detach()` | `tokio::spawn(fut)` (dropping JoinHandle detaches) |
| `smol::unblock(fn)` | `tokio::task::spawn_blocking(fn)` |
| `smol::LocalExecutor::new()` + `ex.run(...)` | `tokio::task::LocalSet::new()` + `local.block_on(&rt, ...)` |
| `ex.spawn(...)` inside LocalSet | `tokio::task::spawn_local(...)` |
| `smol::Timer::after(dur)` | `tokio::time::sleep(dur)` |
| `smol::Timer::at(instant)` | `tokio::time::sleep_until(tokio::time::Instant::from_std(instant))` |
| `smol::Task<T>` | `tokio::task::JoinHandle<T>` |
| `smol::net::TcpListener::bind(...)` | `tokio::net::TcpListener::bind(...)` |
| `smol::net::TcpStream::connect(...)` | `tokio::net::TcpStream::connect(...)` |
| `smol::future::yield_now()` | `tokio::task::yield_now()` |
| `smol::channel::bounded(n)` | `flume::bounded(n)` |
| `smol::channel::unbounded()` | `flume::unbounded()` |
| `isahc::HttpClient::builder()` | `reqwest::Client::builder()` |
| `client.send(request)` (sync) | `rt.block_on(client.execute(request))` |
| `client.send_async(request).await` | `client.execute(request).await` |
| `isahc::Request::builder()` | Use reqwest builder: `client.get(url)`, `client.post(url)`, etc. |
| `response.into_body()` | `response.bytes_stream()` |
| `isahc::config::Configurable::redirect_policy` | `reqwest::ClientBuilder::redirect(...)` |
| `isahc::ReadResponseExt::bytes()` | `response.bytes().await` |
| `futures_lite::io::BufReader` | `tokio::io::BufReader` (for tokio I/O) or `futures::io::BufReader` (for futures I/O) |
| `futures_lite::io::AsyncBufReadExt` | `tokio::io::AsyncBufReadExt` or `futures::io::AsyncBufReadExt` |
| `futures_lite::io::AsyncWriteExt` | `tokio::io::AsyncWriteExt` |
| `futures_lite::future::race(a, b)` | `tokio::select! { r = a => r, r = b => r }` |
| `futures_lite::future::or(a, b)` | `tokio::select! { r = a => r, r = b => r }` |
| `async_process::Command` | `tokio::process::Command` |
| `async_process::Child` | `tokio::process::Child` |
| `async_process::ChildStdin` | `tokio::process::ChildStdin` |
| `async_process::Stdio` | `tokio::process::Stdio` |
| `async_lock::Mutex` | `tokio::sync::Mutex` |
| `async_io::Timer::after(dur)` | `tokio::time::sleep(dur)` |

### 4. Handle SSE streaming

This is the most complex HTTP change. Old code:

```rust
let reader = futures_lite::io::BufReader::new(response.into_body());
let mut lines = reader.lines();
```

New code (for tokio / futures compat):

```rust
use futures::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;

let stream = response.bytes_stream();
let reader = StreamReader::new(stream.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
let mut lines = futures::io::BufReader::new(reader).lines();
```

Note: `StreamReader` from `tokio_util` bridges a `Stream<Bytes>` to `AsyncRead`. Then use `futures::io::BufReader` and `futures::io::AsyncBufReadExt::lines()` because `StreamReader` implements `futures::io::AsyncRead`, not `tokio::io::AsyncRead`.

### 5. Handle tests

Old tests:
```rust
#[test]
fn my_test() {
    smol::block_on(async {
        // test body
    });
}
```

New tests:
```rust
#[tokio::test]
async fn my_test() {
    // test body
}
```

For tests that need `!Send` futures (LocalSet):
```rust
#[tokio::test]
async fn my_test() {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
        // test body
    }).await;
}
```

### 6. Make the call tree async — no `block_on` wrappers

`isahc` had synchronous APIs like `client.send(request)`. `reqwest` is async-only. When a sync function now needs to call async code, make the function itself async and propagate the change up the call tree. Do not use `tokio::runtime::Runtime::block_on()` to squeeze async into sync — push the async boundary up to the entrypoint instead.

```rust
// Wrong — do not wrap in block_on
fn sync_caller() -> Result<T, E> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async_caller())
}

// Right — make the whole tree async
async fn former_sync_caller() -> Result<T, E> {
    async_caller().await
}
```

The entrypoint (`main` / `#[tokio::main]`) provides the runtime. Everything below it is async.

### 7. Verify the result

After translating, run these commands to verify:

```bash
cargo clippy -p <crate> --all-features --tests -- -D warnings
cargo nextest run -p <crate>
```

Fix any compilation errors. Common ones:

- **Wrong `AsyncRead` trait**: `tokio::io::AsyncRead` vs `futures::io::AsyncRead` are different traits. Match the trait to the type.
- **Missing `.await`**: `reqwest` is async-only.
- **Wrong channel type**: `flume::bounded` instead of `smol::channel`.
- **Timer type mismatch**: `tokio::time::sleep` returns a future that needs `.await`.
- **`JoinError` handling**: `tokio::spawn_blocking` returns `Result<Result<T, E>, JoinError>`. Use `??` if needed.

### 8. Keep flume and event-listener

These are already runtime-agnostic. Do not replace them.

## Common Pitfalls

1. **Do not remove `futures-lite` from Cargo.toml until Phase 8 (cleanup).** Some files still need it during the transition (e.g., for `futures_lite::future::pending` in `agent/run.rs`).

2. **Do not change test logic.** Only change the async runtime wrapping the tests. All assertions and test behavior should stay identical.

3. **Be careful with `tokio::select!` scoping.** Variables declared in one branch are not available in others. Use `let result = tokio::select! { ... }` when you need the value.

4. **`tokio::process` vs `async_process`**: Both have similar APIs, but `tokio::process::Child` has `try_wait()` which replaces some manual patterns.

5. **`smol::Task::await` returns the value directly.** `tokio::task::JoinHandle::await` returns `Result<T, JoinError>`.

## Example: Translating a Simple Patch

Old patch adding a timeout to a provider:

```rust
// OLD (smol)
let result = futures_lite::future::or(
    async { client.send_async(request).await },
    async { smol::Timer::after(timeout).await; Err(AgentError::Timeout) }
).await;
```

Translated:

```rust
// NEW (tokio)
let result = tokio::select! {
    r = client.execute(request) => r.map_err(AgentError::from),
    _ = tokio::time::sleep(timeout) => Err(AgentError::Timeout { secs: timeout.as_secs() }),
};
```

## When to Ask for Help

If the patch touches any of these especially complex areas, ask a human for review:

- `maki-lua/src/runtime.rs` (LocalExecutor / LocalSet)
- `maki-agent/src/mcp/stdio.rs` (process I/O + channels)
- SSE streaming in any provider
- OAuth callback server (`maki-agent/src/mcp/oauth/callback.rs`)
- `cancel.rs` (cancellation tokens with racing)
