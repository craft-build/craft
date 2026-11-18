# Quick Start

## Install

Install from the repository (installs the binary as `crafty`):

```sh
cargo install --locked --git https://github.com/craft-build/craft.git craft
```

Or build from a checkout:

```sh
git clone https://github.com/craft-build/craft.git
cd craft
cargo build -p craft --release
# binary at target/release/crafty
```

The self-updater is built in: `crafty update` applies a digest-pinned update and `crafty rollback` undoes one.

## API keys

crafty never stores secrets in its config — `api_key_env` names an environment variable that holds the key:

```sh
export ANTHROPIC_API_KEY=sk-ant-…
# or OPENAI_API_KEY=…, SYNTHETIC_API_KEY=…, ZHIPU_API_KEY=…
```

On first run, crafty auto-detects providers from credential environment variables when the config declares none, so the fastest path to a working agent is: export one key, run `crafty`.

## Run

From your project directory:

```sh
crafty
```

Type a prompt, press **Enter**, and the agent starts working. A startup prompt also works without the TUI:

```sh
crafty "find the flaky retry test and fix it" --print
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

- [Context Management](./context.md) — how crafty keeps the window lean.
- [The Harness](./harness.md) — the layer architecture behind every surface.
- [Permissions & Snapshots](./permissions.md) — approvals, `--yolo`, and `/undo`.
- [Headless Mode](./headless.md) — `--print` for scripts and CI.
