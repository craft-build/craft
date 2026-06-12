+++
title = "ACP"
weight = 9
[extra]
group = "Reference"
+++

# ACP (Agent Client Protocol)

Run Craft as an [Agent Client Protocol](https://github.com/zed-industries/agent-client-protocol) server. Editors like Zed and JetBrains can drive Craft as a coding agent over stdio.

```bash
craft acp
```

The process speaks ACP v1 over stdio (newline-delimited JSON-RPC). It only writes the protocol stream to stdout; logs go to stderr.

## What works

- `initialize` advertises image + embedded-context prompt capabilities and one auth method per configured provider.
- `session/new`, `session/prompt`, `session/cancel`.
- Streaming `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, and a final stop reason.
- `session/request_permission` for any tool that requires a prompt — same gating as the TUI.
- TodoList tool output mirrored as ACP `plan` notifications.
- `session/set_mode` between `build` (default) and `plan`. While in `plan`, mutating tools are blocked, matching TUI behavior.
- Routing through the client when it advertises `fs.read_text_file` / `fs.write_text_file` / `terminal`. Otherwise Craft uses the local filesystem and runs commands locally.

## Modes

| Mode | What it does |
|------|--------------|
| `build` | Default. Full tool surface. |
| `plan` | Read-only research mode. Write-class tools are gated; the agent drafts a plan before any mutation. |

`session/set_mode` cancels any in-flight prompt before switching.

## Auth

Each provider Craft can detect (via env vars or stored credentials) becomes one ACP `auth_method`. Run `craft auth login <provider>` once outside ACP to set it up; sessions then use those credentials.

## Output formats

Tool outputs render to ACP content blocks:

- `read`, `grep`, `glob`, `bash`, etc. → text content.
- `write` / `edit` / `multiedit` / `apply_patch` → structured `Diff` content with old + new text.
- `todowrite` → `plan` notification (no inline content).

Tool kind hints:

| Craft tool | ACP `ToolKind` |
|------------|----------------|
| `read` | `Read` |
| `write`, `edit`, `multiedit`, `apply_patch` | `Edit` |
| `grep`, `glob`, `index` | `Search` |
| `bash`, `code_execution` | `Execute` |
| `webfetch`, `websearch` | `Fetch` |
| `task`, `review`, `check` | `Think` |
| anything else | `Other` |

## Stop reasons

| Craft outcome | ACP `StopReason` |
|---------------|-------------------|
| Done | `end_turn` |
| Cancelled | `cancelled` |
| Context overflow | `max_tokens` |
| Other error | `refusal` (with the error in the last update) |

## Limits

- One prompt per session in v1. Open a new session for the next turn.
- `session/load` is not implemented. Sessions are in-memory only.
- MCP servers configured for the TUI are not yet exposed over ACP.
- No custom ACP extensions (the `x-` namespace).

## Editor wiring

### Zed

Add an external agent in `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "craft": {
      "command": "craft",
      "args": ["acp"]
    }
  }
}
```

Then pick **craft** from the agent picker. Zed forwards file reads / writes and terminal calls through ACP, so edits land in your editor buffers and shell commands appear in Zed's terminal panel.

### Other clients

Any ACP v1 client that can spawn a stdio child process should work. Point it at `craft acp` and let the protocol negotiate capabilities.
