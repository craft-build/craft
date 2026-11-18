# MCP

Use the Model Context Protocol (MCP) to give the agent access to external tools. Craft connects to servers over stdio or HTTP.

Add `mcp "name"` blocks to the global configuration (`~/.config/craft.bml` or `~/.config/craft/*.bml`) or the project's `.craft/mcp.bml`. Blocks are merged; a project block takes precedence over a global block with the same name.

## Stdio transport

Craft starts the program in `command` and communicates with it over standard input and output. Include the program and each argument as separate list items. This example requires Node.js and `npx`:

```bml
mcp "filesystem" {
  command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
  environment = { API_TOKEN = "${MY_MCP_TOKEN}" }
  enabled = true       # default
  timeout = 30000      # ms, 1..=300000
}
```

## HTTP transport

```bml
mcp "remote" {
  url = "https://mcp.example.com/mcp"
  headers = { Authorization = "Bearer ${EXAMPLE_TOKEN}" }
}
```

`headers` are sent with every request to the server. `${VAR}` expands from Craft's process environment; set `EXAMPLE_TOKEN` before starting Craft rather than putting a token in the file.

### OAuth

For servers without a registration endpoint, declare a static OAuth client. The callback listener always binds loopback; `callback_port` fixes the redirect URI's port so it can be pre-registered:

```bml
mcp "acme" {
  url = "https://mcp.acme.example.com/mcp"
  oauth {
    client_id = "acme-client"
    client_secret = "${ACME_CLIENT_SECRET}"
    callback_port = 3118               # fixed loopback port
    callback_path = "/callback"        # default /mcp/oauth/callback
    callback_hostname = "localhost"    # default 127.0.0.1; loopback only
  }
}
```

Every key except `client_id` is optional; `callback_hostname` must resolve to loopback.

## Common settings

- The block name can contain ASCII letters, digits, and hyphens. It must not collide with a built-in tool name.
- `enabled` defaults to `true`; set it to `false` to disable a server.
- `timeout` is in milliseconds, from `1` to `300000`; the default is `30000`.
- `environment` sets variables for a stdio server. `${VAR}` in environment and header values expands from Craft's process environment.

## Use connected tools

- MCP tools are proxied as portable `server__tool` names; permission rules can scope them (`mcp__server` for a whole server — see [Permissions](./permissions.md#tool-policy-flags)).
- `/mcp` in the TUI lists connected servers.
- Craft's built-in [Argosy knowledge layer](./argosy.md) does not need an MCP server.
