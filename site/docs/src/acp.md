# ACP

`craft acp` runs an Agent Client Protocol server on stdio, for editor and IDE integration (e.g. [Zed](https://zed.dev) and other ACP-speaking clients).

```json
{
  "adapter": { "command": "craft", "args": ["acp"] }
}
```

## What the server exposes

- **Sessions** — `session/load` resumes a stored session through the same transcript builder as `/sessions` and `-s` (see [Sessions](./sessions.md)).
- **Permissions** — `session/request_permission` consults the same `PermissionManager` as every other surface; rule evaluation is identical to the TUI and headless gate (see [Permissions](./permissions.md)).
- **Thinking** — a Thinking session option exposes the same controls as `/thinking` in the TUI (see [CLI → Thinking controls](./cli.md#thinking-controls)).
- **Attachments** — client-provided attachments and client MCP servers are supported.

## Configuration

Provider and model selection come from the same BML config as every surface (see [Configuration](./configuration.md)); no editor-specific setup exists beyond pointing the client at the binary.
