# Quick Start

## Install

You need Rust and Cargo to install from source. Install the `craft` binary from the repository:

```sh
cargo install --locked --git https://github.com/craft-build/craft.git craft
```

Or build from a checkout:

```sh
git clone https://github.com/craft-build/craft.git
cd craft
cargo build -p craft --release
./target/release/craft
```

The checkout build puts the binary at `target/release/craft`; it does not add it to your `PATH`. The commands below assume `craft` is on your `PATH`.

To update an installed binary, run `craft update`. To undo one update, run `craft rollback`.

## API keys

Set a provider API key in the shell where you will run Craft. Keep the key out of configuration files: `api_key_env` refers to an environment variable, not a literal key.

```sh
export ANTHROPIC_API_KEY="your-api-key"
```

You can use `OPENAI_API_KEY`, `SYNTHETIC_API_KEY`, or `ZHIPU_API_KEY` instead. When no providers are configured, Craft detects providers from these environment variables. You do not need a configuration file to get started.

## Run

From your project directory:

```sh
craft
```

Type a prompt and press **Enter**. Start with a question about the project, such as “Explain how the tests are organized.”

You can also supply a prompt on the command line. Add `--print` to get a non-interactive run instead of opening the TUI:

```sh
craft "find the flaky retry test and fix it" --print
```

Useful first commands inside the TUI:

- `/model` — pick a model (configured models merge with live listings and the models.dev catalog).
- `/sessions` — list and resume sessions.
- `/usage` and `/stats` — token spend and session stats.
- `Ctrl-H` — keybindings; `Ctrl-P` — command palette.

## Configuration

Configuration lives in BML format at `~/.config/craft.bml` (or split across `~/.config/craft/*.bml`). A minimal provider setup:

```bml
provider "anthropic" {
  kind = "anthropic"
  api_key_env = "ANTHROPIC_API_KEY"
}
```

Local models work too — a keyless loopback endpoint needs no key at all:

```bml
provider "local" {
  kind = "openai-compatible"
  base_url = "http://localhost:8080/v1"
}
```

See [Configuration](./configuration.md) for the full file layout and [Providers](./providers.md) for every provider kind. A fully commented example ships in the repository as [`agent/agent.example.bml`](https://github.com/craft-build/craft/blob/main/agent/agent.example.bml).

## Where to go next

- [Context Management](./context.md) — how craft keeps the window lean.
- [TUI](./tui.md) — plan mode, keyboard shortcuts, and slash commands.
- [Permissions & Snapshots](./permissions.md) — approvals, `--yolo`, and `/undo`.
- [Headless Mode](./headless.md) — `--print` for scripts and CI.
