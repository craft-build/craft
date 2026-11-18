# Development

The CLI harness lives in [`agent/`](https://github.com/craft-build/craft/tree/main/agent) (crate `craft`, binary `craft`), built on the [animist](https://crates.io/crates/animist) provider/agent/history stack with the `argosy` knowledge crate linked in-process.

## Build and test

Run these commands from the repository root:

```sh
cargo build -p craft
cargo test -p craft
cargo fmt -p craft --check
cargo clippy -p craft --all-targets -- -D warnings
```

`ProviderHandle::from_config` builds only the selected animist provider; parsing configuration never constructs clients or initiates interactive authentication.

## Provider conformance (offline)

`agent/src/conformance/` tests provider kinds through a real `Conversation` against local mock HTTP servers. It covers Anthropic Messages, OpenAI Responses/Chat Completions, and Bedrock Converse with AWS eventstream framing.

Checks include text, thinking, tool-call round trips, tool-result image placement, retry/authentication/overflow recovery, Bedrock retries, usage, and catalog pricing. These tests are part of `cargo test -p craft` and need no credentials.

## Live smoke tests (opt-in)

Live tests make real model calls and may incur provider charges. They are ignored by default and require explicit environment configuration. For a keyless local server:

```sh
CRAFT_LIVE_BASE_URL=http://127.0.0.1:8000/v1 \
CRAFT_LIVE_MODEL=orion-26b \
  cargo test -p craft --lib conformance::live -- --ignored --nocapture
```

If the server needs authentication, supply `CRAFT_LIVE_API_KEY` through your environment. Do not put credentials in a committed script or configuration file.

`CRAFT_LIVE_KIND` selects the config kind (default `openai-compatible`; `llama-cpp`, `omlx`, `ollama`, `anthropic`, ...). The two tests: a plain text round trip, and a tool round trip that must read a file and quote it back.

## Manual QA

For manual QA, configure a local model with a `provider` block using the same `kind` and `base_url`, then run the TUI. Exercise tool calls, cancellation (Esc), session resume (`/sessions`), and compaction (`/compact`). The `CRAFT_LIVE_*` variables configure the tests, not the TUI.

## This site

The docs you are reading live in [`site/`](https://github.com/craft-build/craft/tree/main/site) — a static landing page plus this mdBook, built by `site/build.sh` and deployed on push to `main` via GitHub Pages.

To build just the docs from the repository root:

```sh
mdbook build site/docs
```

To assemble the landing page and docs together, run the site script from its directory:

```sh
cd site
./build.sh
```
