//! `mcp.bml` loading and validation (B.11).
//!
//! Global config (`mcp "name" { ... }` blocks in the merged craft document)
//! is merged first, then the project's `.craft/mcp.bml` overrides by server
//! name. Expansion errors
//! (unset `${VAR}`) fail the individual server, not the whole file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::error::McpError;
use crate::permissions::is_valid_server_name;
use crate::tools::is_builtin_tool;

const MCP_CONFIG_FILE: &str = "mcp.bml";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone)]
pub enum McpConfigError {
    Read { path: PathBuf, error: String },
    Parse { path: PathBuf, error: String },
}

/// Generates a compacted but still human-meaningful version of a path.
fn compact_path(path: &Path, base_path: &Path) -> String {
    let mut path_string = path.to_string_lossy().into_owned();

    if let Ok(stripped) = path.strip_prefix(base_path) {
        path_string = format!(".{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    }
    if !path_string.starts_with('.')
        && let Some(home) = crate::paths::home()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        path_string = format!("~{}{}", std::path::MAIN_SEPARATOR, stripped.display());
    }

    path_string
}

/// Wraps a `Vec` of `McpConfigError`s for compact display.
#[derive(Clone, Debug)]
pub struct McpConfigErrors {
    errors: Vec<McpConfigError>,
    initial_wd: PathBuf,
}

impl McpConfigErrors {
    pub fn new(working_directory: PathBuf) -> Self {
        McpConfigErrors {
            errors: Vec::new(),
            initial_wd: working_directory,
        }
    }

    fn add_error(&mut self, e: McpConfigError) {
        self.errors.push(e);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for McpConfigErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown = self.errors.iter().take(2).map(|e| match e {
            McpConfigError::Read { path, .. } => {
                format!("failed to read {}", compact_path(path, &self.initial_wd))
            }
            McpConfigError::Parse { path, .. } => {
                format!("failed to parse {}", compact_path(path, &self.initial_wd))
            }
        });
        if let Some(first) = shown.next() {
            write!(f, "{first}")?;
            for rest in shown {
                write!(f, "; {rest}")?;
            }
        }
        let hidden = self.errors.len().saturating_sub(2);
        if hidden > 0 {
            write!(f, "; ... ({hidden} more)")?;
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerStatus {
    Connecting,
    Running,
    Disabled,
    Failed(String),
    NeedsAuth { url: Option<String> },
}

impl McpServerStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Connecting)
    }
}

#[derive(Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub transport_kind: &'static str,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub resource_count: usize,
    pub status: McpServerStatus,
    pub config_path: PathBuf,
    pub url: Option<String>,
    pub oauth: Option<OauthClientConfig>,
}

#[derive(Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub mcp: HashMap<String, RawServerConfig>,
    #[serde(skip)]
    pub origins: HashMap<String, PathBuf>,
}

#[derive(Deserialize, Clone)]
pub struct RawServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(flatten)]
    pub transport: RawTransport,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum RawTransport {
    Stdio(RawStdioFields),
    Http(RawHttpFields),
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RawStdioFields {
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RawHttpFields {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub oauth: Option<OauthClientConfig>,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub name: String,
    pub timeout: Duration,
    pub transport: Transport,
}

/// Static OAuth client used when the server has no registration endpoint.
#[derive(Deserialize, Clone, Debug)]
pub struct OauthClientConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Fixed loopback port so the redirect URI can be pre-registered.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Loopback path of the redirect URI (defaults to `/mcp/oauth/callback`).
    #[serde(default)]
    pub callback_path: Option<String>,
    /// Loopback hostname of the redirect URI (defaults to `127.0.0.1`).
    #[serde(default)]
    pub callback_hostname: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Transport {
    Stdio {
        program: String,
        args: Vec<String>,
        environment: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
        oauth: Option<OauthClientConfig>,
    },
}

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty()
    }
}

/// Expands `${VAR}` references from the process environment. An unset or empty
/// variable is an error carrying the variable name.
pub(crate) fn expand_env(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let var = &after[..end];
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => out.push_str(&v),
            _ => return Err(var.to_string()),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Erroring (not dropping) surfaces the variable name in the server's status
/// instead of an untraceable 401 later.
fn expand_map(
    server: &str,
    kind: &str,
    map: HashMap<String, String>,
) -> Result<HashMap<String, String>, McpError> {
    map.into_iter()
        .map(|(key, value)| {
            let expanded = expand_env(&value).map_err(|var| {
                McpError::Config {
                    message: format!(
                        "server '{server}' {kind} '{key}': environment variable '{var}' is unset or empty"
                    ),
                }
            })?;
            Ok((key, expanded))
        })
        .collect()
}

/// The OAuth callback listener must bind loopback only; anything else would
/// expose the auth-flow endpoint to the network.
fn is_loopback_callback_hostname(hostname: &str) -> bool {
    if matches!(hostname, "localhost" | "::1" | "[::1]") {
        return true;
    }
    hostname
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|addr| addr.is_loopback())
}

pub fn parse_server(name: String, server: RawServerConfig) -> Result<ServerConfig, McpError> {
    if !is_valid_server_name(&name) {
        return Err(McpError::Config {
            message: format!("server name '{name}' must be ASCII alphanumeric + hyphens"),
        });
    }
    if is_builtin_tool(&name) {
        return Err(McpError::Config {
            message: format!("server name '{name}' conflicts with built-in tool"),
        });
    }
    if server.timeout == 0 || server.timeout > MAX_TIMEOUT_MS {
        return Err(McpError::Config {
            message: format!("server '{name}' timeout must be 1..={MAX_TIMEOUT_MS}"),
        });
    }
    let transport = match server.transport {
        RawTransport::Stdio(cfg) => {
            let mut cmd = cfg.command.into_iter();
            let program = cmd.next().ok_or(McpError::Config {
                message: format!("server '{name}' has empty command"),
            })?;
            Transport::Stdio {
                program,
                args: cmd.collect(),
                environment: expand_map(&name, "environment", cfg.environment)?,
            }
        }
        RawTransport::Http(cfg) => {
            if !cfg.url.starts_with("http://") && !cfg.url.starts_with("https://") {
                return Err(McpError::Config {
                    message: format!("server '{name}' url must start with http:// or https://"),
                });
            }
            if let Some(path) = cfg.oauth.as_ref().and_then(|o| o.callback_path.as_ref())
                && (path.is_empty() || !path.starts_with('/'))
            {
                return Err(McpError::Config {
                    message: format!("server '{name}' oauth.callback_path must start with '/'"),
                });
            }
            if let Some(host) = cfg
                .oauth
                .as_ref()
                .and_then(|o| o.callback_hostname.as_ref())
                && !is_loopback_callback_hostname(host)
            {
                return Err(McpError::Config {
                    message: format!(
                        "server '{name}' oauth.callback_hostname '{host}' must be a loopback address (localhost, 127.0.0.0/8, or ::1)"
                    ),
                });
            }
            Transport::Http {
                url: cfg.url,
                headers: expand_map(&name, "header", cfg.headers)?,
                oauth: cfg.oauth,
            }
        }
    };
    Ok(ServerConfig {
        name,
        timeout: Duration::from_millis(server.timeout),
        transport,
    })
}

pub fn transport_kind(raw: &RawTransport) -> &'static str {
    match raw {
        RawTransport::Stdio(_) => "stdio",
        RawTransport::Http(_) => "http",
    }
}

pub fn load_config(cwd: &Path) -> (McpConfig, McpConfigErrors) {
    let mut merged = McpConfig::default();
    let mut errors = McpConfigErrors::new(cwd.to_path_buf());

    // Global servers come from the merged craft document's `mcp` blocks.
    let loaded = crate::bml::load_global();
    for (path, error) in loaded.errors {
        errors.add_error(McpConfigError::Parse {
            path,
            error: error.to_string(),
        });
    }
    let global_path = crate::paths::config_search_dirs()
        .into_iter()
        .next_back()
        .map(|dir| dir.join(MCP_CONFIG_FILE))
        .unwrap_or_default();
    if let Some(doc) = loaded.doc.as_ref() {
        match servers_from_doc(doc) {
            Ok(servers) => {
                tracing::info!(servers = servers.len(), "loaded mcp config");
                for name in servers.keys() {
                    merged.origins.insert(name.clone(), global_path.clone());
                }
                merged.mcp.extend(servers);
            }
            Err(error) => errors.add_error(McpConfigError::Parse {
                path: global_path,
                error,
            }),
        }
    } else {
        crate::bml::warn_legacy_toml(false);
    }

    let project_path = cwd.join(".craft").join(MCP_CONFIG_FILE);
    match read_config(&project_path) {
        Ok(None) => {}
        Ok(Some(cfg)) => {
            tracing::info!(
                path = %project_path.display(),
                servers = cfg.mcp.len(),
                "loaded mcp config"
            );
            for name in cfg.mcp.keys() {
                merged.origins.insert(name.clone(), project_path.clone());
            }
            merged.mcp.extend(cfg.mcp);
        }
        Err(e) => errors.add_error(e),
    }
    (merged, errors)
}

pub fn persist_enabled(
    config_path: &Path,
    server_name: &str,
    enabled: bool,
) -> Result<(), McpError> {
    let mut doc = crate::bml::parse_file_or_empty(config_path)
        .map_err(|e| McpError::Config { message: e })?;
    let server = crate::bml::ensure_labeled_block(&mut doc, "mcp", server_name);
    crate::bml::upsert_assign(server, "enabled", crate::bml::bool_value(enabled));

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| McpError::Config {
            message: format!("cannot create dir: {e}"),
        })?;
    }
    fs::write(config_path, crate::bml::to_text(&doc)).map_err(|e| McpError::Config {
        message: format!("cannot write {}: {e}", config_path.display()),
    })?;
    Ok(())
}

/// Extract `mcp "name" { ... }` blocks from a document.
fn servers_from_doc(doc: &barkml::Statement) -> Result<HashMap<String, RawServerConfig>, String> {
    let mut servers = HashMap::new();
    for (id, labels, block) in doc.blocks() {
        if id != "mcp" {
            continue;
        }
        let Some(name) = labels.first().and_then(|l| l.as_string().cloned()) else {
            continue;
        };
        let json = crate::bml::container_json(block);
        let raw: RawServerConfig =
            serde_json::from_value(json).map_err(|e| format!("mcp server {name:?}: {e}"))?;
        servers.insert(name, raw);
    }
    Ok(servers)
}

fn read_config(path: &Path) -> Result<Option<McpConfig>, McpConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %path.display(), "no mcp config to read");
            return Ok(None);
        }
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to read mcp config");
            return Err(McpConfigError::Read {
                path: path.into(),
                error: e.to_string(),
            });
        }
    };
    let doc = if content.trim().is_empty() {
        None
    } else {
        match crate::bml::parse(&content) {
            Ok(doc) => Some(doc),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse mcp config");
                return Err(McpConfigError::Parse {
                    path: path.into(),
                    error: e.to_string(),
                });
            }
        }
    };
    let mcp = match doc.as_ref().map(servers_from_doc) {
        None => HashMap::new(),
        Some(Ok(servers)) => servers,
        Some(Err(error)) => {
            tracing::warn!(path = %path.display(), error = %error, "invalid mcp config");
            return Err(McpConfigError::Parse {
                path: path.into(),
                error,
            });
        }
    };
    Ok(Some(McpConfig {
        mcp,
        origins: HashMap::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn parse_mcp(text: &str) -> McpConfig {
        let doc = crate::bml::parse(text).unwrap();
        McpConfig {
            mcp: servers_from_doc(&doc).unwrap(),
            origins: HashMap::new(),
        }
    }

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Stdio(RawStdioFields {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                environment: HashMap::new(),
            }),
        }
    }

    fn http_raw(url: &str) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Http(RawHttpFields {
                url: url.to_string(),
                headers: HashMap::new(),
                oauth: None,
            }),
        }
    }

    #[test_case("srv", stdio_raw(&[]), "empty command" ; "empty_command")]
    #[test_case("bash", stdio_raw(&["echo"]), "conflicts with built-in" ; "builtin_name_collision")]
    #[test_case("bad name!", stdio_raw(&["echo"]), "ASCII alphanumeric" ; "invalid_server_name")]
    #[test_case("srv", http_raw("ftp://bad.com"), "http://" ; "invalid_http_url")]
    fn parse_server_rejects(name: &str, cfg: RawServerConfig, expected_msg: &str) {
        let err = parse_server(name.into(), cfg).unwrap_err();
        assert!(err.to_string().contains(expected_msg), "got: {err}");
    }

    #[test]
    fn header_with_unset_var_fails_the_server_with_the_var_name() {
        let config = parse_mcp(
            "mcp \"remote\" { url = \"https://mcp.example.com/mcp\"\n  headers = { Authorization = \"Bearer ${CRAFT_TEST_MCP_UNSET_84421}\" } }",
        );
        let err = parse_server("remote".into(), config.mcp["remote"].clone()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CRAFT_TEST_MCP_UNSET_84421"), "got: {msg}");
        assert!(msg.contains("Authorization"), "got: {msg}");
    }

    #[test]
    fn header_with_empty_var_fails_like_unset() {
        unsafe { std::env::set_var("CRAFT_TEST_MCP_EMPTY_84421", "") };
        let config = parse_mcp(
            "mcp \"remote\" { url = \"https://mcp.example.com/mcp\"\n  headers = { Authorization = \"Bearer ${CRAFT_TEST_MCP_EMPTY_84421}\" } }",
        );
        let err = parse_server("remote".into(), config.mcp["remote"].clone()).unwrap_err();
        assert!(err.to_string().contains("CRAFT_TEST_MCP_EMPTY_84421"));
    }

    #[test]
    fn stdio_environment_expands_from_the_process_env() {
        unsafe { std::env::set_var("CRAFT_TEST_MCP_ENV_84421", "tok") };
        let config = parse_mcp(
            "mcp \"local\" { command = [\"server\"]\n  environment = { GITHUB_TOKEN = \"${CRAFT_TEST_MCP_ENV_84421}\" } }",
        );
        let parsed = parse_server("local".into(), config.mcp["local"].clone()).unwrap();
        match parsed.transport {
            Transport::Stdio { environment, .. } => {
                assert_eq!(environment["GITHUB_TOKEN"], "tok");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test_case(0 ; "zero")]
    #[test_case(MAX_TIMEOUT_MS + 1 ; "over_max")]
    fn invalid_timeout_rejected(timeout: u64) {
        let mut cfg = stdio_raw(&["echo"]);
        cfg.timeout = timeout;
        let err = parse_server("srv".into(), cfg).unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn parse_splits_command_into_program_and_args() {
        let result = parse_server("srv".into(), stdio_raw(&["npx", "-y", "server"])).unwrap();
        match &result.transport {
            Transport::Stdio { program, args, .. } => {
                assert_eq!(program, "npx");
                assert_eq!(args, &["-y", "server"]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn bml_deserialization() {
        let bml_str = r#"
            mcp "filesystem" {
              command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
            }

            mcp "github" {
              command = ["gh", "mcp-server"]
              environment = { GITHUB_TOKEN = "tok" }
              timeout = 10000
              enabled = false
            }

            mcp "remote" {
              url = "https://mcp.example.com/mcp"
              headers = { Authorization = "Bearer tok123" }
            }
        "#;
        let config = parse_mcp(bml_str);
        assert_eq!(config.mcp.len(), 3);

        assert!(matches!(
            config.mcp["filesystem"].transport,
            RawTransport::Stdio(_)
        ));

        let gh_cfg = &config.mcp["github"];
        assert!(!gh_cfg.enabled);
        assert_eq!(gh_cfg.timeout, 10_000);
        match &gh_cfg.transport {
            RawTransport::Stdio(s) => assert_eq!(s.environment["GITHUB_TOKEN"], "tok"),
            _ => panic!("expected Stdio"),
        }

        match &config.mcp["remote"].transport {
            RawTransport::Http(h) => {
                assert_eq!(h.url, "https://mcp.example.com/mcp");
                assert_eq!(h.headers["Authorization"], "Bearer tok123");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_config_deserializes() {
        let config = parse_mcp(
            "mcp \"acme\" { url = \"https://mcp.acme.example.com/mcp\"\n  oauth { client_id = \"acme-client\"\n    client_secret = \"s3cret\"\n    callback_port = 3118\n    callback_path = \"/callback\" } }",
        );
        let parsed = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { url, oauth, .. } => {
                assert_eq!(url, "https://mcp.acme.example.com/mcp");
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_id, "acme-client");
                assert_eq!(oauth.client_secret.as_deref(), Some("s3cret"));
                assert_eq!(oauth.callback_port, Some(3118));
                assert_eq!(oauth.callback_path.as_deref(), Some("/callback"));
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_client_secret_and_port_optional() {
        let config = parse_mcp(
            "mcp \"acme\" { url = \"https://mcp.acme.example.com/mcp\"\n  oauth { client_id = \"acme-client\" } }",
        );
        let parsed = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap();
        match parsed.transport {
            Transport::Http { oauth, .. } => {
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_secret, None);
                assert_eq!(oauth.callback_port, None);
                assert_eq!(oauth.callback_path, None);
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn oauth_callback_path_must_start_with_slash() {
        let config = parse_mcp(
            "mcp \"acme\" { url = \"https://mcp.acme.example.com/mcp\"\n  oauth { client_id = \"acme-client\"\n    callback_path = \"callback\" } }",
        );
        let err = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap_err();
        assert!(err.to_string().contains("callback_path"));
    }

    #[test]
    fn oauth_callback_hostname_must_be_loopback() {
        for bad in [
            "0.0.0.0",
            "evil.example.com",
            "127.0.0.1@evil.com",
            "10.0.0.5",
        ] {
            let config = parse_mcp(&format!(
                "mcp \"acme\" {{ url = \"https://mcp.acme.example.com/mcp\"\n  oauth {{ client_id = \"acme-client\"\n    callback_hostname = \"{bad}\" }} }}",
            ));
            let err = parse_server("acme".into(), config.mcp["acme"].clone()).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("callback_hostname"), "bad={bad} got: {msg}");
            assert!(msg.contains("loopback"), "bad={bad} got: {msg}");
        }
        for good in ["localhost", "127.0.0.1", "127.1.2.3", "::1", "[::1]"] {
            let config = parse_mcp(&format!(
                "mcp \"acme\" {{ url = \"https://mcp.acme.example.com/mcp\"\n  oauth {{ client_id = \"acme-client\"\n    callback_hostname = \"{good}\" }} }}",
            ));
            parse_server("acme".into(), config.mcp["acme"].clone())
                .unwrap_or_else(|e| panic!("good={good} got: {e}"));
        }
    }

    fn raw_block_error(text: &str) -> String {
        let doc = crate::bml::parse(text).unwrap();
        match servers_from_doc(&doc) {
            Ok(_) => panic!("expected a config error"),
            Err(e) => e,
        }
    }

    #[test]
    fn mixed_stdio_and_http_keys_are_a_hard_error() {
        let err = raw_block_error(
            "mcp \"srv\" { command = [\"server\"]\n  url = \"https://mcp.example.com/mcp\" }",
        );
        assert!(err.contains("did not match any variant"), "got: {err}");
    }

    #[test]
    fn unknown_key_in_stdio_block_errors() {
        let err = raw_block_error("mcp \"srv\" { command = [\"server\"]\n  bogus = 1 }");
        assert!(err.contains("did not match any variant"), "got: {err}");
    }

    #[test]
    fn unknown_key_in_http_block_errors() {
        let err =
            raw_block_error("mcp \"srv\" { url = \"https://mcp.example.com/mcp\"\n  bogus = 1 }");
        assert!(err.contains("did not match any variant"), "got: {err}");
    }

    #[test]
    fn project_config_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let global_dir = dir.path().join("global");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("mcp.bml"),
            "mcp \"srv\" { command = [\"global\"]\n  timeout = 5000 }",
        )
        .unwrap();

        let project_dir = dir.path().join("project");
        let project_craft_dir = project_dir.join(".craft");
        fs::create_dir_all(&project_craft_dir).unwrap();
        fs::write(
            project_craft_dir.join("mcp.bml"),
            "mcp \"srv\" { command = [\"project\"] }",
        )
        .unwrap();

        let project_cfg = read_config(&project_craft_dir.join("mcp.bml"))
            .unwrap()
            .unwrap();
        let global_cfg = read_config(&global_dir.join("mcp.bml")).unwrap().unwrap();

        let mut merged = McpConfig::default();
        merged.mcp.extend(global_cfg.mcp);
        merged.mcp.extend(project_cfg.mcp);

        let all: Vec<_> = merged
            .mcp
            .into_iter()
            .filter(|(_, v)| v.enabled)
            .map(|(name, cfg)| parse_server(name, cfg))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(all.len(), 1);
        match &all[0].transport {
            Transport::Stdio { program, .. } => assert_eq!(program, "project"),
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn persist_enabled_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.bml");

        // Creates the file from scratch.
        persist_enabled(&path, "srv", false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = false"), "{text}");
        assert!(text.contains("mcp \"srv\""), "{text}");

        // Preserves other fields and flips the flag.
        fs::write(
            &path,
            "mcp \"srv\" { command = [\"echo\"]\n  timeout = 5000\n  enabled = true }",
        )
        .unwrap();
        persist_enabled(&path, "srv", false).unwrap();
        let cfg = read_config(&path).unwrap().unwrap();
        assert!(!cfg.mcp["srv"].enabled);
        match &cfg.mcp["srv"].transport {
            RawTransport::Stdio(s) => assert_eq!(s.command, vec!["echo"]),
            _ => panic!("expected Stdio"),
        }
        assert_eq!(cfg.mcp["srv"].timeout, 5000);
    }

    #[test]
    fn read_config_directory_path_returns_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.bml");
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            read_config(&path),
            Err(McpConfigError::Read { .. })
        ));
    }

    #[test]
    fn read_config_invalid_toml_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.bml");
        fs::write(&path, "this is not valid bml {{").unwrap();
        assert!(matches!(
            read_config(&path),
            Err(McpConfigError::Parse { .. })
        ));
    }

    #[test]
    fn read_config_valid_bml_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.bml");
        fs::write(&path, "mcp \"valid\" { command = [\"echo\", \"hello\"] }").unwrap();
        let cfg = read_config(&path).unwrap().unwrap();
        assert!(cfg.mcp.contains_key("valid"));
    }
}
