# Architecture Migration: smol → tokio & isahc → reqwest

This document provides a complete reference for migrating the maki codebase from the smol async runtime to tokio, and from isahc to reqwest for HTTP. It is structured so that an implementing agent can execute the migration in phases, one crate at a time, with full context at each step.

## Motivation

- **smol → tokio**: tokio is the de-facto standard Rust async runtime with broader ecosystem support, better tooling, richer feature set (IO driver, filesystem, net, signal, process, sync), and more mature behavior under load. smol's ecosystem is smaller and less maintained.
- **isahc → reqwest**: reqwest is the most widely used Rust HTTP client, with better test coverage, more robust TLS handling, streaming support, and native tokio integration. isahc wraps curl and brings unnecessary complexity plus a heavy C dependency.

## Scope

All 12 workspace crates are affected. The migration touches approximately 50+ source files.

## Current Async Ecosystem

| Crate | Purpose |
|---|---|
| `smol` (v2) | Async runtime: `block_on`, `spawn`, `LocalExecutor`, `Timer`, `unblock`, `net::Tcp*` |
| `futures-lite` (v2) | `AsyncRead`/`AsyncBufRead`/`AsyncWrite` traits, `future::race`/`or`, `StreamExt`, `io::BufReader` |
| `async-process` (v2) | `Command`, `Child`, `ChildStdin`, `ChildStdout`, `ChildStderr`, `Stdio` |
| `async-lock` (v3) | `Mutex` (async-aware) |
| `async-io` (v2) | `Timer` (used directly in `child_guard.rs`) |
| `async-channel` | Bounded/unbounded channels (used via `smol::channel`) |
| `event-listener` (v5) | `Event` for custom notification patterns (`cancel.rs`, `InflightGate`) |
| `isahc` (v1.7) | HTTP client: `HttpClient`, `Request`, `AsyncBody`, `Response`, SSE streaming |
| `flume` (v0.11) | Multi-producer multi-consumer channels with async support |

## Target Ecosystem

| Replacement | Purpose |
|---|---|
| `tokio` (v1, features: `full` or granular) | Async runtime: `runtime::Runtime`, `spawn`, `time::sleep`/`sleep_until`, `net::Tcp*`, `task::spawn_blocking` |
| `tokio-util` (v0.7, features: `io`, `compat`) | `io::StreamReader`, `compat` bridge for futures-lite traits during transition |
| `reqwest` (v0.12) | HTTP client: `Client`, `RequestBuilder`, `Response`, streaming |
| `futures` (v0.3) | `future::select`, `StreamExt`, `AsyncRead`/`AsyncWrite`/`AsyncBufRead` traits |
| `tokio` (built-in) | `process::Command`, `process::Child`, `process::ChildStdin/Stdout/Stderr` |
| `tokio` (built-in) | `sync::Mutex`, `sync::mpsc`, `sync::broadcast`, `sync::watch` |
| Keep `flume` | Unchanged - flume is runtime-agnostic, works fine with tokio |
| Keep `event-listener` | Unchanged - also runtime-agnostic |

## Key API Translation Reference

### Runtime

| smol | tokio |
|---|---|
| `smol::block_on(future)` | `tokio::runtime::Runtime::new().block_on(future)` or `#[tokio::main]` |
| `smol::spawn(future)` | `tokio::spawn(future)` |
| `smol::unblock(closure)` | `tokio::task::spawn_blocking(closure)` |
| `smol::LocalExecutor::new()` | `tokio::task::LocalSet` + `local_set.block_on(&rt, future)` |
| `smol::Timer::after(dur)` | `tokio::time::sleep(dur)` |
| `smol::Timer::at(instant)` | `tokio::time::sleep_until(instant.into())` |

### Channels

| smol | tokio |
|---|---|
| `smol::channel::bounded(n)` | `tokio::sync::mpsc::channel(n)` or keep `flume::bounded(n)` |
| `smol::channel::unbounded()` | `tokio::sync::mpsc::unbounded_channel()` or keep `flume::unbounded()` |
| `sender.send(val).await` | `sender.send(val).await` (same for flume) |
| `receiver.recv().await` | `receiver.recv().await` (returns `Option<T>` for mpsc, `Result<T, RecvError>` for flume) |

**Decision: Keep `flume` everywhere.** Flume is runtime-agnostic, already deeply integrated, and has both sync+async APIs. Replacing flume with tokio::sync::mpsc is unnecessary churn that changes semantics (flume receivers are `Clone`, tokio mpsc receivers are not). Only `smol::channel` (used in `mcp/stdio.rs`) needs replacement — replace those with `flume::bounded`.

### Async I/O Traits

| futures-lite | tokio/futures |
|---|---|
| `futures_lite::io::AsyncRead` | `tokio::io::AsyncRead` (or `futures::io::AsyncRead` via `tokio-util::compat`) |
| `futures_lite::io::AsyncBufRead` | `futures::io::AsyncBufRead` |
| `futures_lite::io::AsyncWrite` | `tokio::io::AsyncWrite` |
| `futures_lite::io::BufReader` | `tokio::io::BufReader` |
| `futures_lite::io::AsyncBufReadExt::lines()` | `futures::io::AsyncBufReadExt::lines()` |
| `futures_lite::future::race(a, b)` | `futures::future::select(a, b)` (pin-based, different API) |
| `futures_lite::future::or(a, b)` | `futures::future::select(a, b)` |

**Critical: `futures::future::select` has a different signature.** It returns `Either<(T1, F2), (T2, F1)>` instead of just the winner's output. Every call site needs updating.

### Process

| async-process | tokio |
|---|---|
| `async_process::Command` | `tokio::process::Command` |
| `async_process::Child` | `tokio::process::Child` |
| `async_process::ChildStdin` | `tokio::process::ChildStdin` |
| `async_process::ChildStdout` | `tokio::process::ChildStdout` |
| `async_process::Stdio::piped()` | `tokio::process::Stdio::piped()` |

### Networking

| smol | tokio |
|---|---|
| `smol::net::TcpListener::bind(addr).await` | `tokio::net::TcpListener::bind(addr).await` |
| `smol::net::TcpStream::connect(addr).await` | `tokio::net::TcpStream::connect(addr).await` |
| `listener.accept().await` | `listener.accept().await` (same) |

### HTTP Client

| isahc | reqwest |
|---|---|
| `isahc::HttpClient::builder().timeout(d).build()` | `reqwest::Client::builder().timeout(d).build()` |
| `isahc::HttpClient::builder().connect_timeout(d)` | `reqwest::Client::builder().connect_timeout(d)` |
| `client.send(request)` (sync) | `client.execute(request).await` (reqwest is async-only) |
| `client.send_async(request).await` | `client.execute(request).await` |
| `isahc::Request::builder().method("POST").uri(url).body(body)` | `client.request(method, url).body(body)` |
| `isahc::config::Configurable::redirect_policy` | `reqwest::Client::builder().redirect(reqwest::redirect::Policy::...)` |
| `response.status()` | `response.status()` (same) |
| `response.text().await` | `response.text().await` (same) |
| `response.bytes().await` | `response.bytes().await` (returns `Bytes` not `Vec<u8>`) |
| `isahc::AsyncBody::from(bytes)` | `reqwest::Body::from(bytes)` |
| `isahc::Response<isahc::AsyncBody>` | `reqwest::Response` |
| `response.into_body()` → `AsyncRead` | `response.bytes_stream()` → `Stream<Item=Result<Bytes>>` |
| `BufReader::new(response.into_body()).lines()` | `response.bytes_stream()` + custom line framing |
| `isahc::http::header::HeaderMap` | `reqwest::header::HeaderMap` |
| `isahc::http::Request` | `reqwest::Request` (or use builder pattern directly) |

**Critical: SSE streaming.** The biggest isahc → reqwest change is streaming. isahc gives an `AsyncRead` body; reqwest gives a `Stream<Item = Result<Bytes>>`. SSE parsing currently reads lines from `BufReader::new(response.into_body())`. With reqwest, you need to convert the byte stream into a line-based reader. Options:
1. `tokio_util::io::StreamReader` to convert `Stream<Bytes>` → `AsyncRead`, then wrap in `BufReader`
2. Direct stream processing with manual line buffering

Option 1 is recommended for minimal SSE logic changes.

### Locking

| async-lock | tokio |
|---|---|
| `async_lock::Mutex::new(val)` | `tokio::sync::Mutex::new(val)` |
| `lock.lock().await` | `lock.lock().await` (same API) |

### Yield

| smol | tokio |
|---|---|
| `smol::future::yield_now().await` | `tokio::task::yield_now().await` |

---

## File Inventory: What Touches What

### smol usage by crate

**Root binary crate** (`src/`):
- `src/print.rs` — `smol::block_on`, `smol::Timer::after`, `futures_lite::future::or`
- `src/cmd/subcmd.rs` — `smol::block_on`
- `src/update.rs` — `isahc::get` (sync), `isahc::ReadResponseExt`
- `src/error.rs` — `isahc::AsyncReadResponseExt`

**maki-agent**:
- `maki-agent/src/task_set.rs` — `smol::Task`, `smol::spawn`, `smol::block_on`
- `maki-agent/src/cancel.rs` — `smol::spawn`, `smol::Timer::after`, `futures_lite::future::race`
- `maki-agent/src/child_guard.rs` — `async_process::Child`, `async_io::Timer`, `futures_lite::future::or`
- `maki-agent/src/headless.rs` — `smol::Task`, `smol::spawn`, `smol::block_on`
- `maki-agent/src/mcp/stdio.rs` — `async_process::{Command, ChildStdin, Stdio}`, `smol::channel::bounded`, `futures_lite::io::{BufReader, AsyncBufReadExt, AsyncWriteExt}`, `async_lock::Mutex`
- `maki-agent/src/mcp/http.rs` — `isahc::{HttpClient, Request}`, `isahc::config::Configurable`, `smol::unblock`, `async_lock::Mutex`
- `maki-agent/src/mcp/mod.rs` — `smol::spawn`, `smol::unblock`, `smol::Timer::after`, `futures_lite::future::or`, `async_lock::Mutex`
- `maki-agent/src/mcp/oauth/callback.rs` — `smol::net::TcpListener`, `smol::net::TcpStream`, `smol::Timer::after`, `smol::spawn`, `smol::block_on`, `futures_lite::AsyncWriteExt`, `futures_lite::AsyncReadExt`, `futures_lite::future::race`
- `maki-agent/src/mcp/oauth/discovery.rs` — `isahc::HttpClient`, `isahc::http::Request`, `smol::unblock`
- `maki-agent/src/mcp/oauth/token.rs` — `isahc::HttpClient`, `isahc::http::Request`, `smol::unblock`
- `maki-agent/src/mcp/oauth/registration.rs` — `isahc::HttpClient`, `isahc::http::Request`
- `maki-agent/src/mcp/oauth/mod.rs` — `isahc::HttpClient`, `isahc::config::Configurable`
- `maki-agent/src/agent/streaming.rs` — `futures_lite::future::race`, `smol::Timer::after`
- `maki-agent/src/agent/run.rs` — `futures_lite::future::race`, `smol::Timer::after`, `async_lock::Mutex`
- `maki-agent/src/agent/compaction.rs` — `smol::block_on`
- `maki-agent/src/agent/tool_dispatch.rs` — (via transitive deps)
- `maki-agent/src/tools/mod.rs` — `async_lock::Mutex`
- `maki-agent/src/tools/task.rs` — `async_lock::Mutex`
- `maki-agent/src/tools/code_execution.rs` — `async_lock::Mutex`
- `maki-agent/src/tools/edit.rs` — (async deps)
- `maki-agent/src/tools/write.rs` — (async deps)
- `maki-agent/src/tools/read.rs` — (async deps)
- `maki-agent/src/tools/grep.rs` — (async deps)
- `maki-agent/src/tools/multiedit.rs` — (async deps)
- `maki-agent/src/tools/batch.rs` — (async deps)

**maki-providers**:
- `maki-providers/src/provider.rs` — `smol::unblock`, `smol::spawn`
- `maki-providers/src/providers/mod.rs` — `isahc::HttpClient`, `isahc::config::Configurable`, `futures_lite::io::AsyncBufRead`, `futures_lite::StreamExt`, `smol::Timer::after`, `futures_lite::future::or`
- `maki-providers/src/providers/anthropic/mod.rs` — `isahc::{HttpClient, Request, AsyncBody, Response}`, `futures_lite::io::BufReader`, `smol::block_on` (tests)
- `maki-providers/src/providers/anthropic/bedrock.rs` — `isahc::{HttpClient, Request}`, `isahc::config::Configurable`, `isahc::ReadResponseExt`
- `maki-providers/src/providers/openai_compat.rs` — `isahc::{HttpClient, Request, AsyncReadResponseExt}`, `futures_lite::io::{AsyncBufRead, AsyncBufReadExt, BufReader}`, `smol::block_on` (tests)
- `maki-providers/src/providers/openai/responses.rs` — `isahc::{HttpClient, Request}`, `futures_lite::io::{AsyncBufRead, BufReader, Cursor}`, `smol::block_on` (tests)
- `maki-providers/src/providers/openai/auth.rs` — `isahc::HttpClient`, `isahc::ReadResponseExt`, `isahc::config::Configurable`, `isahc::Request`
- `maki-providers/src/providers/google.rs` — `isahc::{HttpClient, Request, AsyncReadResponseExt}`, `futures_lite::io::{AsyncBufReadExt, BufReader}`, `smol::block_on` (tests)
- `maki-providers/src/providers/copilot/mod.rs` — `isahc::{HttpClient, Request, AsyncReadResponseExt}`, `futures_lite::io::BufReader`
- `maki-providers/src/providers/dynamic.rs` — (transitive)

**maki-lua**:
- `maki-lua/src/runtime.rs` — `smol::LocalExecutor`, `smol::block_on`, `smol::Timer::{after, at}`, `smol::spawn`, `smol::future::yield_now`, `futures_lite::future::race`, `event_listener::Event`
- `maki-lua/src/api/net.rs` — `isahc::{AsyncBody, HttpClient, Request}`, `isahc::config::{Configurable, RedirectPolicy}`, `futures_lite::io::AsyncReadExt`
- `maki-lua/src/api/async_api.rs` — (async deps)
- `maki-lua/src/api/fn_api.rs` — `futures_lite::pin`, `futures_lite::future::or`, `async_channel`
- `maki-lua/src/api/tool.rs` — `futures_lite::future::race`, `smol::block_on` (tests)
- `maki-lua/src/api/fs.rs` — (async deps)
- `maki-lua/src/api/ui.rs` — (async deps)
- `maki-lua/tests/plugin_host.rs` — (test deps)

**maki-ui**:
- `maki-ui/src/event_loop.rs` — `smol::Task`, `smol::spawn`
- `maki-ui/src/agent/agent_loop.rs` — `smol::unblock`, `smol::spawn`, `async_lock::Mutex`
- `maki-ui/src/agent/mod.rs` — `smol::block_on`, `smol::spawn`, `smol::Timer::after`, `futures_lite::future::or`
- `maki-ui/src/agent/command_router.rs` — `smol::spawn`
- `maki-ui/src/update.rs` — `smol::spawn`
- `maki-ui/src/app/shell.rs` — `async_process::{Command, Stdio}`, `futures_lite::io::{AsyncBufReadExt, BufReader}`, `futures_lite::future::race`, `futures_lite::StreamExt`
- `maki-ui/src/app/btw.rs` — `futures_lite::future`

**maki-storage**:
- `maki-storage/src/version.rs` — `isahc::{HttpClient, Request, AsyncReadResponseExt, ReadResponseExt}`, `isahc::config::Configurable`, `isahc::http::Error`

**maki-docgen**:
- `maki-docgen/src/main.rs` — `smol::block_on`, `smol::unblock`, `smol::future::zip`

### isahc usage by crate

| Crate | Files | isahc APIs Used |
|---|---|---|
| root `src/` | `update.rs`, `error.rs` | `isahc::get`, `ReadResponseExt`, `AsyncReadResponseExt`, `isahc::Error` |
| `maki-agent` | `mcp/http.rs`, `mcp/oauth/{mod,discovery,token,registration,callback}.rs` | `HttpClient`, `Request`, `Configurable`, `http::header`, `http::Method`, `http::StatusCode` |
| `maki-providers` | `providers/{mod,anthropic/mod,anthropic/bedrock,openai/auth,openai/responses,openai_compat,google,copilot/mod}.rs` | `HttpClient`, `Request`, `AsyncReadResponseExt`, `ReadResponseExt`, `Configurable`, `AsyncBody`, `Response` |
| `maki-storage` | `version.rs` | `HttpClient`, `Request`, `AsyncReadResponseExt`, `ReadResponseExt`, `Configurable`, `http::Error` |
| `maki-lua` | `api/net.rs` | `AsyncBody`, `HttpClient`, `Request`, `Configurable`, `RedirectPolicy`, `AsyncReadExt` |

---

## Risks and Gotchas

1. **LocalExecutor → LocalSet**: `smol::LocalExecutor` runs `!Send` futures on a single thread. `tokio::task::LocalSet` does the same but requires a `Runtime` handle. The Lua runtime in `maki-lua/src/runtime.rs` depends heavily on `LocalExecutor` for cooperative scheduling. This is the most complex part of the migration.

2. **`smol::channel` → `flume`**: The MCP stdio transport uses `smol::channel::bounded(1)` for pending response senders. These must become `flume::bounded(1)` since flume's `Sender::send` is `async` (matching `smol::channel::Sender::send`).

3. **SSE streaming**: All provider implementations (Anthropic, OpenAI, Google, Copilot) parse SSE by reading lines from `BufReader::new(response.into_body())` where `into_body()` returns an `AsyncRead`. With reqwest, `response.bytes_stream()` returns `impl Stream<Item = Result<Bytes>>`. Use `tokio_util::io::StreamReader` to bridge.

4. **`futures_lite::future::race` / `or` → `tokio::select!`**: These are used extensively for timeout patterns. The idiomatic tokio equivalent is `tokio::select!`, but it's a macro with different scoping rules. Alternatively, use `futures::future::select` which returns `Either`.

5. **`smol::unblock` → `tokio::task::spawn_blocking`**: Semantics differ slightly. `spawn_blocking` returns a `JoinHandle` and runs on a dedicated thread pool. Error handling is different (returns `JoinError` if the pool panics).

6. **`isahc::http` types → `reqwest` types**: isahc re-exports `http` crate types (`http::Request`, `http::Response`, `http::Method`, `http::StatusCode`, `http::header::*`). reqwest has its own types. The `http` crate types can still be used independently, but reqwest's builder API is different.

7. **Sync HTTP**: `isahc` supports synchronous `client.send(request)`. `reqwest` is async-only. The sync call sites (`src/update.rs`, `maki-storage/src/version.rs::fetch_latest`, `maki-providers/src/providers/openai/auth.rs`) need to be wrapped in a runtime or made async.

8. **Test helpers**: Many tests use `smol::block_on(async { ... })` inline. These all need to become `#[tokio::test]` or `tokio::test::block_on`.

9. **`smol::Task`**: Used in `task_set.rs`, `event_loop.rs`, `headless.rs`. tokio's equivalent is `tokio::task::JoinHandle`. `smol::Task` is `Send` and has `.detach()`. `JoinHandle` is also `Send` but uses `tokio::spawn`.

10. **`async_io::Timer`**: Used directly in `child_guard.rs` and `mcp/stdio.rs`. Replace with `tokio::time::sleep`.
