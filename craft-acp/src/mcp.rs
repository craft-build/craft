use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol_schema::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use craft_agent::mcp::config::{ServerConfig, Transport};

const SEPARATOR: &str = "__";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn convert_acp_servers(servers: &[McpServer]) -> Vec<ServerConfig> {
    let mut results = Vec::with_capacity(servers.len());
    let mut seen_names: HashMap<String, usize> = HashMap::new();

    for server in servers {
        match convert_one(server) {
            Ok(mut cfg) => {
                cfg.name = sanitize_name(&cfg.name, &mut seen_names);
                results.push(cfg);
            }
            Err(e) => {
                tracing::warn!(error = %e, "skipping invalid ACP MCP server");
            }
        }
    }
    results
}

fn convert_one(server: &McpServer) -> Result<ServerConfig, String> {
    match server {
        McpServer::Stdio(s) => convert_stdio(s),
        McpServer::Http(s) => convert_http(s),
        McpServer::Sse(s) => convert_sse(s),
        _ => Err("unsupported MCP server transport".into()),
    }
}

fn convert_stdio(s: &McpServerStdio) -> Result<ServerConfig, String> {
    let name = sanitize_chars(&s.name);
    if name.is_empty() {
        return Err("stdio server has empty name after sanitization".into());
    }
    let program = s.command.to_string_lossy().to_string();
    if program.is_empty() {
        return Err(format!("stdio server '{name}' has empty command"));
    }
    let environment: HashMap<String, String> = s
        .env
        .iter()
        .map(|EnvVariable { name, value, .. }| (name.clone(), value.clone()))
        .collect();
    Ok(ServerConfig {
        name,
        timeout: DEFAULT_TIMEOUT,
        transport: Transport::Stdio {
            program,
            args: s.args.clone(),
            environment,
        },
    })
}

fn convert_http(s: &McpServerHttp) -> Result<ServerConfig, String> {
    let name = sanitize_chars(&s.name);
    if name.is_empty() {
        return Err("http server has empty name after sanitization".into());
    }
    validate_url(&name, &s.url)?;
    let headers: HashMap<String, String> = s
        .headers
        .iter()
        .map(|HttpHeader { name, value, .. }| (name.clone(), value.clone()))
        .collect();
    Ok(ServerConfig {
        name,
        timeout: DEFAULT_TIMEOUT,
        transport: Transport::Http {
            url: s.url.clone(),
            headers,
        },
    })
}

fn convert_sse(s: &McpServerSse) -> Result<ServerConfig, String> {
    let name = sanitize_chars(&s.name);
    if name.is_empty() {
        return Err("sse server has empty name after sanitization".into());
    }
    validate_url(&name, &s.url)?;
    let headers: HashMap<String, String> = s
        .headers
        .iter()
        .map(|HttpHeader { name, value, .. }| (name.clone(), value.clone()))
        .collect();
    Ok(ServerConfig {
        name,
        timeout: DEFAULT_TIMEOUT,
        transport: Transport::Http {
            url: s.url.clone(),
            headers,
        },
    })
}

fn validate_url(name: &str, url: &str) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!(
            "server '{name}' url must start with http:// or https://"
        ));
    }
    Ok(())
}

fn sanitize_chars(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}

fn sanitize_name(name: &str, seen: &mut HashMap<String, usize>) -> String {
    let base = sanitize_chars(name);
    if !base.contains(SEPARATOR) && base.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            return base;
        }
        return format!("{base}-{}", *count);
    }
    let base = base.replace(SEPARATOR, "-");
    let count = seen.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        return base;
    }
    format!("{base}-{}", *count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_stdio_server() {
        let server = McpServer::Stdio(
            McpServerStdio::new("my-server", "/usr/bin/mcp")
                .args(vec!["--port".into(), "8080".into()])
                .env(vec![EnvVariable::new("KEY", "val")]),
        );
        let configs = convert_acp_servers(&[server]);
        assert_eq!(configs.len(), 1);
        let cfg = &configs[0];
        assert_eq!(cfg.name, "my-server");
        match &cfg.transport {
            Transport::Stdio {
                program,
                args,
                environment,
            } => {
                assert_eq!(program, "/usr/bin/mcp");
                assert_eq!(args, &["--port", "8080"]);
                assert_eq!(environment["KEY"], "val");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn converts_http_server() {
        let server = McpServer::Http(
            McpServerHttp::new("remote", "https://mcp.example.com")
                .headers(vec![HttpHeader::new("Authorization", "Bearer tok")]),
        );
        let configs = convert_acp_servers(&[server]);
        assert_eq!(configs.len(), 1);
        match &configs[0].transport {
            Transport::Http { url, headers } => {
                assert_eq!(url, "https://mcp.example.com");
                assert_eq!(headers["Authorization"], "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn converts_sse_to_http_transport() {
        let server = McpServer::Sse(
            McpServerSse::new("events", "https://example.com/sse")
                .headers(vec![HttpHeader::new("X-Custom", "val")]),
        );
        let configs = convert_acp_servers(&[server]);
        assert_eq!(configs.len(), 1);
        match &configs[0].transport {
            Transport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/sse");
                assert_eq!(headers["X-Custom"], "val");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn sanitizes_special_chars_in_name() {
        let server = McpServer::Stdio(McpServerStdio::new("my server!", "/bin/echo"));
        let configs = convert_acp_servers(&[server]);
        assert_eq!(configs[0].name, "my-server-");
    }

    #[test]
    fn deduplicates_names() {
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("srv", "/bin/a")),
            McpServer::Stdio(McpServerStdio::new("srv", "/bin/b")),
            McpServer::Stdio(McpServerStdio::new("srv", "/bin/c")),
        ];
        let configs = convert_acp_servers(&servers);
        assert_eq!(configs.len(), 3);
        assert_eq!(configs[0].name, "srv");
        assert_eq!(configs[1].name, "srv-2");
        assert_eq!(configs[2].name, "srv-3");
    }

    #[test]
    fn skips_invalid_url() {
        let server = McpServer::Http(McpServerHttp::new("bad", "ftp://nope.com"));
        let configs = convert_acp_servers(&[server]);
        assert!(configs.is_empty());
    }

    #[test]
    fn skips_empty_command() {
        let server = McpServer::Stdio(McpServerStdio::new("x", ""));
        let configs = convert_acp_servers(&[server]);
        assert!(configs.is_empty());
    }
}
