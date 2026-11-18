# MCP

Connect external tool servers over stdio or HTTP. MCP blocks merge from the global config (`~/.config/craft.bml` and `~/.config/craft/*.bml`) and the project's `.craft/mcp.bml` — the project wins on name conflicts.

## Stdio transport

`command` is the program plus arguments:

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

`headers` (with `${VAR}` expansion) rides every request to the server.

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

## Details

- Names are ASCII alphanumeric + hyphens and must not collide with built-in tools.
- `${VAR}` in `environment`/`headers` values expands from the process environment.
- MCP tools are proxied as portable `server__tool` names; permission rules can scope them (`mcp__server` for a whole server — see [Permissions](./permissions.md#tool-policy-flags)).
- `/mcp` in the TUI lists connected servers.
- Craft's own knowledge layer does **not** ride MCP — argosy is linked in-process (see [Argosy](./argosy.md)).
