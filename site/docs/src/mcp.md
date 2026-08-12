# MCP (Model Context Protocol)

Craft connects to external tool servers over MCP. Both **stdio** and **HTTP** transports are supported.

## Configuration

Add servers under `[mcp.*]` in your MCP config:

- **Global**: `~/.config/craft/mcp.toml`
- **Project**: `.craft/mcp.toml` (project config wins when both set a value)

### Stdio

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "ghp_xxxx" }
timeout = 10000
enabled = false
```

### HTTP

```toml
[mcp.analytics]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
```

Some HTTP servers need OAuth but have no dynamic client registration. For those, give Craft a static client:

```toml
[mcp.acme]
url = "https://mcp.acme.example.com/mcp"
oauth = { client_id = "acme-client", client_secret = "s3cret", callback_port = 3118, callback_path = "/callback", callback_hostname = "localhost" }
```

### All options

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `command` | array | | Stdio: program + args |
| `url` | string | | HTTP: server URL |
| `environment` | map | | Stdio only |
| `headers` | map | | HTTP only |
| `oauth` | table | | HTTP only: static client (`client_id`, optional `client_secret`, optional `callback_port`, optional `callback_path`, optional `callback_hostname`) |
| `timeout` | u64 | 30000 | Milliseconds (1-300000) |
| `enabled` | bool | true | |

Set `command` for stdio, `url` for HTTP. Pick one.

## Naming and namespacing

Server names are ASCII alphanumeric, hyphens ok. Tools get prefixed with their server name: a `read` tool on the `filesystem` server becomes `filesystem__read`. Because of this, `__` is reserved and names can't collide with built-in tools.

## Runtime toggling

Turn servers on/off from the MCP picker in the UI (`/mcp`). Changes save back to your config.

## Status

| Status | Meaning |
|--------|---------|
| Connecting | Waiting for the server to come up |
| Running | Tools available |
| Disabled | Off in config or toggled off in UI |
| Failed | Error shown in UI |
| NeedsAuth | Waiting for OAuth (see below) |

If one server fails, the rest still work.

## OAuth

Some HTTP servers need auth. When that happens, Craft opens your browser to log in. Other servers keep working while you authenticate. Tokens refresh on their own. If you change the server URL, you log in again.

```bash
craft mcp auth <server-name>     # manually trigger auth
craft mcp logout <server-name>   # remove stored tokens
```

Servers without dynamic client registration need a client you registered yourself (say, your own app on their platform). Add it to the server config so the auth flow uses it instead of trying to register:

| Field | Type | Notes |
|-------|------|-------|
| `client_id` | string | Client id of your registered app |
| `client_secret` | string | Optional, for confidential clients |
| `callback_port` | u16 | Optional, pins the loopback port so the redirect URI can be pre-registered |
| `callback_path` | string | Optional, loopback path of the redirect URI (default `/mcp/oauth/callback`) |
| `callback_hostname` | string | Optional, loopback hostname of the redirect URI (default `127.0.0.1`) |

Set `callback_port` when the server only accepts exact redirect URIs. Otherwise Craft falls back to its default port, then to any free port, so the redirect URI changes between runs. Set `callback_path` when the server registered a different path (for example, `/callback`). Set `callback_hostname` to `localhost` when the server registered the name form instead of the IP (the listener still binds to 127.0.0.1).

### Headless machines

On a machine without a browser (say, a dev server over SSH), run `craft mcp auth <server-name>`. Craft prints the login URL. Open it on your laptop and log in. The browser lands on a `http://127.0.0.1:19876/...` page that fails to load. Copy that full URL from the address bar and paste it into the terminal to finish the login.

## Prompts

MCP servers can expose prompts (reusable message templates). Craft shows them as slash commands in the command palette: `/server:prompt-name`. Type `/` to filter.

```
/github:create-pr           # no arguments
/analytics:report monthly   # one argument
/review:code src tests      # multiple, positional
```

Skip a required argument and Craft shows a usage hint. Prompts are fetched at startup and on reconnect, so new ones need a restart. Only text content is supported.
