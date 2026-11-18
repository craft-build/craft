# Development

The CLI harness lives in [`agent/`](https://github.com/craft-build/craft/tree/main/agent) (crate `craft`, binary `crafty`), built on the [animist](https://crates.io/crates/animist) provider/agent/history stack with the `argosy` knowledge crate linked in-process.

## Build and test

```sh
cargo test -p craft
cargo fmt -p craft --check
cargo clippy -p craft --all-targets -- -D warnings
```

`ProviderHandle::from_config` builds only the selected animist provider; parsing configuration never constructs clients or initiates interactive authentication.

## Provider conformance (offline)

`src/conformance/` drives every supported kind through a real `Conversation` against a local mock HTTP server speaking each protocol (Anthropic Messages, OpenAI Responses/Chat Completions, Bedrock Converse with AWS eventstream framing): text, thinking and tool-call round trips, tool-result image placement, retry/reauth/overflow recovery, Bedrock's in-provider retries, usage, and catalog pricing. It is part of `cargo test` and needs no credentials.

## Live smoke tests (opt-in)

Real model calls, `#[ignore]`d and env-gated so CI never pays for inference:

```sh
CRAFT_LIVE_BASE_URL=http://127.0.0.1:8000/v1 \
CRAFT_LIVE_MODEL=orion-26b \
CRAFT_LIVE_API_KEY=…            # optional for keyless local servers
  cargo test -p craft --lib conformance::live -- --ignored --nocapture
```

`CRAFT_LIVE_KIND` selects the config kind (default `openai-compatible`; `llama-cpp`, `omlx`, `ollama`, `anthropic`, ...). The two tests: a plain text round trip, and a tool round trip that must read a file and quote it back.

## Manual QA

The same env serves a manual QA session: run the TUI against a local model (a `provider` block with the same `kind`/`base_url`), exercise tool calls, cancel (Esc), resume (`/sessions`), and `/compact`.

## This site

The docs you are reading live in [`site/`](https://github.com/craft-build/craft/tree/main/site) — a static landing page plus this mdBook, built by `site/build.sh` and deployed on push to `main` via GitHub Pages.
