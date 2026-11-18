# ACP

`craft acp` runs an Agent Client Protocol (ACP) server over standard input and output. Use it to connect an ACP-capable editor or IDE, such as [Zed](https://zed.dev), to Craft.

## Connect an editor

Configure your client's agent command as `craft`, with `acp` as its argument:

```sh
craft acp
```

Use your client's ACP configuration format; there is no universal editor JSON schema. Make sure the client can find `craft` on its `PATH` and inherits the provider credential environment variables.

## What the server exposes

- **Sessions** — `session/load` resumes a stored session through the same transcript builder as `/sessions` and `-s` (see [Sessions](./sessions.md)).
- **Permissions** — `session/request_permission` consults the same `PermissionManager` as every other surface; rule evaluation is identical to the TUI and headless gate (see [Permissions](./permissions.md)).
- **Thinking** — a Thinking session option exposes the same controls as `/thinking` in the TUI (see [CLI → Thinking controls](./cli.md#thinking-controls)).
- **Attachments** — client-provided attachments and client MCP servers are supported.

## Configuration

Provider and model selection use the same [BML configuration](./configuration.md) as the TUI and headless runs. Permission requests are presented by the ACP client; Craft does not install a local auto-review permission responder for ACP sessions.
