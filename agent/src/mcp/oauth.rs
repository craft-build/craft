//! OAuth 2.1 login flow for HTTP MCP servers (B.11).
//!
//! rmcp provides the protocol machinery (discovery, PKCE, dynamic
//! registration, token refresh); this module supplies the interactive bits —
//! the localhost callback server, browser launch, and a file-backed
//! credential store under the state dir — plus the seam the transport uses to
//! reuse stored tokens on later connections.

use std::path::PathBuf;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};

use super::config::OauthClientConfig;
use super::error::McpError;

const DEFAULT_CALLBACK_PATH: &str = "/mcp/oauth/callback";
const DEFAULT_CALLBACK_HOSTNAME: &str = "127.0.0.1";
/// Login that never completes on its own is not a login.
const CALLBACK_TIMEOUT_SECS: u64 = 300;

/// Where a server's credentials live: `<state>/mcp-oauth/<server>.json`.
pub fn credentials_path(state_dir: &crate::storage::StateDir, server: &str) -> PathBuf {
    state_dir
        .path()
        .join("mcp-oauth")
        .join(format!("{server}.json"))
}

/// File-backed [`CredentialStore`]: tokens survive restarts and are shared
/// between the login flow and later transports.
#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

fn store_error(path: &std::path::Path, what: &str, e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(format!("{what} {}: {e}", path.display()))
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(store_error(&self.path, "read", e)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| store_error(&self.path, "parse", e))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| store_error(parent, "create", e))?;
        }
        let bytes = serde_json::to_vec_pretty(&credentials)
            .map_err(|e| store_error(&self.path, "serialize", e))?;
        crate::storage::atomic_write(&self.path, &bytes)
            .map_err(|e| store_error(&self.path, "write", e))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(store_error(&self.path, "remove", e)),
        }
    }
}

/// A manager wired to the server's stored credentials, if any exist yet. Used
/// by the HTTP transport path so a reconnect picks fresh tokens up from disk
/// without credentials traveling through the command channel.
pub async fn stored_manager(
    server: &str,
    url: &str,
    state_dir: &crate::storage::StateDir,
) -> Option<AuthorizationManager> {
    let store = FileCredentialStore::new(credentials_path(state_dir, server));
    match store.load().await {
        Ok(Some(_)) => {}
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(server, error = %e, "cannot read stored OAuth credentials");
            return None;
        }
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| tracing::warn!(server, error = %e, "OAuth manager construction failed"))
        .ok()?;
    manager.set_credential_store(store);
    Some(manager)
}

fn oerr(server: &str, reason: impl std::fmt::Display) -> McpError {
    McpError::OAuthFailed {
        server: server.to_string(),
        reason: reason.to_string(),
    }
}

/// The URL the user should visit. `login` opens it automatically when it can.
#[derive(Debug, Clone)]
pub struct LoginInfo {
    pub auth_url: String,
}

/// Run the interactive authorization-code flow for one HTTP server:
/// discover metadata, (pre-)register the client, start the loopback callback
/// server, open the browser, exchange the code, persist the tokens.
///
/// `challenge` is the `WWW-Authenticate` header captured from the 401 that
/// put the server into `NeedsAuth`; seeding discovery from it skips probing.
pub async fn login(
    server: &str,
    url: &str,
    oauth: Option<&OauthClientConfig>,
    challenge: Option<&str>,
) -> Result<LoginInfo, McpError> {
    let state_dir = crate::storage::StateDir::resolve()
        .map_err(|e| oerr(server, format!("no state dir for credential storage: {e}")))?;
    let store = FileCredentialStore::new(credentials_path(&state_dir, server));

    // Callback endpoint: fixed port (pre-registrable) or ephemeral.
    let host = oauth
        .and_then(|o| o.callback_hostname.clone())
        .unwrap_or_else(|| DEFAULT_CALLBACK_HOSTNAME.to_string());
    let path = oauth
        .and_then(|o| o.callback_path.clone())
        .unwrap_or_else(|| DEFAULT_CALLBACK_PATH.to_string());
    let bind = async |port: Option<u16>| match port {
        Some(port) => tokio::net::TcpListener::bind((host.as_str(), port)).await,
        None => tokio::net::TcpListener::bind((host.as_str(), 0)).await,
    };
    let listener = bind(oauth.and_then(|o| o.callback_port))
        .await
        .map_err(|e| oerr(server, format!("cannot bind callback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| oerr(server, format!("callback socket has no local addr: {e}")))?
        .port();
    let redirect_uri = format!("http://{host}:{port}{path}");

    let mut request = AuthorizationRequest::new(redirect_uri).with_client_name("craft");
    if let Some(oauth) = oauth {
        request = request.with_preregistered_client(oauth.client_id.clone());
        if let Some(secret) = &oauth.client_secret {
            request = request.with_client_secret(secret.clone());
        }
    }
    if let Some(challenge) = challenge {
        request = request.with_challenge(challenge);
    }

    let mut oauth_state = OAuthState::new(url.to_string(), None)
        .await
        .map_err(|e| oerr(server, format!("OAuth init failed: {e}")))?;
    if let OAuthState::Unauthorized(manager) = &mut oauth_state {
        manager.set_credential_store(store);
    }
    oauth_state
        .start_authorization(request)
        .await
        .map_err(|e| oerr(server, format!("cannot start authorization: {e}")))?;

    let auth_url = match &oauth_state {
        OAuthState::Session(session) => session.auth_url.clone(),
        _ => return Err(oerr(server, "authorization did not produce a session")),
    };

    let info = LoginInfo {
        auth_url: auth_url.clone(),
    };
    open_browser(&auth_url);

    // Serve exactly one callback request: GET <path>?code=..&state=..
    let callback_url = tokio::time::timeout(
        std::time::Duration::from_secs(CALLBACK_TIMEOUT_SECS),
        serve_callback(listener, &path, &host),
    )
    .await
    .map_err(|_| oerr(server, "login timed out waiting for the browser callback"))?
    .map_err(|e| oerr(server, format!("callback failed: {e}")))?;

    oauth_state
        .handle_callback_url(&callback_url)
        .await
        .map_err(|e| oerr(server, format!("token exchange failed: {e}")))?;
    oauth_state
        .complete_authorization()
        .await
        .map_err(|e| oerr(server, format!("cannot complete authorization: {e}")))?;

    tracing::info!(server, "MCP OAuth login complete");
    Ok(info)
}

/// Accept one HTTP request and return its full redirect URL. The URL is
/// rebuilt with the listener's actual host/port so strict IdPs comparing
/// redirect URIs during the code exchange see the registered one.
async fn serve_callback(
    listener: tokio::net::TcpListener,
    path: &str,
    host: &str,
) -> Result<String, std::io::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = listener.local_addr()?.port();
    loop {
        let (mut socket, _peer) = listener.accept().await?;
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        // Read until end of the request headers.
        loop {
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let Some(request_line) = head.lines().next() else {
            continue;
        };
        let Some(target) = request_line.split(' ').nth(1) else {
            continue;
        };
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\
                  Connection: close\r\n\r\nLogged in",
            )
            .await;
        let _ = socket.shutdown().await;
        if let Some(query) = target.strip_prefix(path) {
            let query = query.trim_start_matches('?');
            return Ok(format!("http://{host}:{port}{path}?{query}"));
        }
        // Wrong path: keep waiting for the real callback request.
    }
}

/// Program + argv used to open `url` in the system default browser.
///
/// The URL is server-derived (an OAuth authorization endpoint), so it must
/// never be re-parsed by a shell: `cmd /C start <url>` runs the URL through
/// cmd.exe tokenization, where `&`, `|`, `&&`, etc. are command separators —
/// a crafted `state`/`prompt` parameter could smuggle a second command.
/// Every branch here is a direct spawn with the URL as a single argv element;
/// `os` is a parameter so the arg shape is unit-testable off-platform.
fn browser_launcher(os: &str, url: &str) -> (&'static str, Vec<String>) {
    match os {
        // rundll32 dispatches to the shell's file-protocol handler for the
        // URL scheme with no cmd.exe parsing in between.
        "windows" => (
            "rundll32",
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        ),
        "macos" => ("open", vec![url.to_string()]),
        _ => ("xdg-open", vec![url.to_string()]),
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    const OS: &str = "windows";
    #[cfg(target_os = "macos")]
    const OS: &str = "macos";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    const OS: &str = "other";

    let (program, args) = browser_launcher(OS, url);
    if std::process::Command::new(program)
        .args(args)
        .spawn()
        .is_err()
    {
        tracing::warn!(url, "could not open a browser; visit the URL manually");
    }
}

/// Convenience for the UI: run [`login`] for a `NeedsAuth` server using its
/// published info, then ask the manager to reconnect so the fresh tokens take
/// effect.
pub async fn login_and_reconnect(
    handle: &super::McpHandle,
    reader: &super::McpSnapshotReader,
    server: &str,
) -> Result<LoginInfo, McpError> {
    let snapshot = reader.load_full();
    let Some(info) = snapshot.infos.iter().find(|i| i.name == server) else {
        return Err(oerr(server, "unknown server"));
    };
    let Some(url) = info.url.clone() else {
        return Err(oerr(server, "server has no HTTP url"));
    };
    let challenge = match &info.status {
        super::config::McpServerStatus::NeedsAuth { url } => url.clone(),
        _ => None,
    };
    let login_info = login(server, &url, info.oauth.as_ref(), challenge.as_deref()).await?;
    handle.send(super::McpCommand::Reconnect {
        server: server.to_string(),
    });
    Ok(login_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credential_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path().join("cred.json"));
        assert!(store.load().await.unwrap().is_none());

        let credentials =
            StoredCredentials::new("client-id".into(), None, vec!["mcp:read".into()], Some(1));
        store.save(credentials).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-id");
        assert_eq!(loaded.granted_scopes, vec!["mcp:read".to_string()]);

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        // Clearing a missing file is fine.
        store.clear().await.unwrap();
    }

    #[tokio::test]
    async fn serve_callback_returns_the_redirect_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_callback(listener, "/cb", "127.0.0.1"));

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        sock.write_all(b"GET /cb?code=a&state=b HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut body = String::new();
        sock.read_to_string(&mut body).await.unwrap();
        assert!(body.contains("Logged in"));

        let url = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}/cb?code=a&state=b"));
    }

    #[test]
    fn windows_launcher_never_routes_through_a_shell() {
        // A server-derived URL containing cmd.exe metacharacters must reach
        // the browser as one inert argv element, not shell syntax.
        let url = "https://auth.example/authorize?state=a&evil=1&calc.exe";
        let (program, args) = browser_launcher("windows", url);
        assert_eq!(program, "rundll32");
        assert_eq!(args, ["url.dll,FileProtocolHandler", url]);
    }

    #[test]
    fn unix_launchers_pass_the_url_as_a_single_argv_element() {
        for os in ["macos", "linux", "other"] {
            let url = "https://auth.example/cb?a=1&b=2";
            let (program, args) = browser_launcher(os, url);
            assert!(
                matches!(program, "open" | "xdg-open"),
                "unexpected launcher {program} for {os}"
            );
            assert_eq!(args, [url]);
        }
    }
}
