use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{mpsc, watch};

use super::config::{
    McpConfig, McpConfigErrors, McpServerInfo, McpServerStatus, OauthClientConfig, ServerConfig,
    Transport, load_config, parse_server, transport_kind,
};
use super::error::McpError;
use super::session::start_session;
use super::*;

/// Budget for draining in-flight toggle-persist writes during shutdown. Kept
/// short on purpose: a persist is a small read-modify-write of a config file,
/// so exceeding this means a wedged filesystem, not a slow write — better to
/// warn and exit than blow the caller's shutdown budget.
const PERSIST_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Window in which `list_changed` notifications coalesce into one refresh.
/// Servers legitimately burst these (a config reload fires tools and prompts
/// together); re-listing per notification would hammer `tools/list`.
pub(super) const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Returns as soon as the config is read, so nothing with a screen waits on a
/// slow `initialize`. Await `McpHandle::ready` before touching the tool index.
pub async fn start(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    start_with_events(cwd, McpEvents::default()).await
}

/// [`start`] with notification callbacks (TUI notices, integration hooks).
pub async fn start_with_events(
    cwd: &Path,
    events: McpEvents,
) -> (Option<McpHandle>, McpConfigErrors) {
    tracing::info!(cwd = %cwd.display(), "starting MCP");
    let cwd = cwd.to_owned();
    let (config, config_errors) = tokio::task::spawn_blocking({
        let cwd = cwd.clone();
        move || load_config(&cwd)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to load config");
        (McpConfig::default(), McpConfigErrors::new(PathBuf::new()))
    });
    // The workspace root backs `roots/list`; callers that passed their own
    // keep theirs.
    let mut events = events;
    events.root = events.root.or_else(|| Some(cwd));
    (start_with_config_events(config, events), config_errors)
}

/// `start` for callers with no frame to protect, who want the tools up front.
/// The wait is bounded (10s, mirroring the TUI's turn gate) so one hung server
/// cannot stall a headless run; a server that lands later simply misses the
/// initial tool registration.
pub async fn start_connected(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    const CONNECTED_BUDGET: Duration = Duration::from_secs(10);
    let (handle, config_errors) = start(cwd).await;
    // Headless: nothing will drain server-initiated requests, so deny them
    // at the source instead of parking the handler forever.
    if let Some(handle) = &handle {
        handle.drop_server_requests();
    }
    if let Some(handle) = &handle {
        if tokio::time::timeout(CONNECTED_BUDGET, handle.ready())
            .await
            .is_err()
        {
            tracing::warn!(
                budget = ?CONNECTED_BUDGET,
                "MCP servers did not settle in time; continuing without the stragglers"
            );
        }
    }
    (handle, config_errors)
}

pub fn start_with_config(config: McpConfig) -> Option<McpHandle> {
    let handle = start_with_config_events(config, McpEvents::default());
    // No frontend: server-initiated requests deny instead of parking.
    if let Some(handle) = &handle {
        handle.drop_server_requests();
    }
    handle
}

/// `start_with_config` with notification callbacks. The session layer's
/// callbacks send into a channel the loop selects on, so a notification
/// storm can never block rmcp's transport task (which invokes them).
pub fn start_with_config_events(config: McpConfig, events: McpEvents) -> Option<McpHandle> {
    if config.is_empty() {
        tracing::info!("no MCP servers configured, skipping");
        return None;
    }

    let inner = parse_entries(config);

    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let index: Arc<ArcSwap<ToolIndex>> = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    publish(&inner, &index, &snapshot);

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = watch::channel(false);
    // Server-initiated requests (sampling, elicitation) ride their own
    // channel to whichever frontend takes the receiver. Headless callers
    // never take it; they drop it so the handler denies instead of parking.
    let (req_tx, req_rx) = mpsc::unbounded_channel::<McpServerRequest>();
    // Session-signaled refreshes ride a dedicated channel into the loop;
    // `McpCommand` stays caller-facing. Senders throttle per server at the
    // debounce window (map bounded by the server count) so a pathological
    // notification storm cannot grow the unbounded channel; a throttled
    // notification schedules a trailing emit, so the burst's final state is
    // still refreshed. The loop's deadline is set when the first pending
    // server lands and never extended, so a continuous stream still flushes
    // within one window.
    let last_sent: Arc<Mutex<HashMap<String, std::time::Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let wired = McpEvents {
        on_list_changed: Some(Arc::new({
            let tx = event_tx.clone();
            let user = events.on_list_changed.clone();
            let last_sent = Arc::clone(&last_sent);
            move |server| {
                if let Some(cb) = &user {
                    cb(server);
                }
                send_list_changed(&tx, &last_sent, server);
            }
        })),
        on_log: events.on_log.clone(),
        on_dead: Some(Arc::new({
            let tx = event_tx.clone();
            let user = events.on_dead.clone();
            move |server| {
                if let Some(cb) = &user {
                    cb(server);
                }
                let _ = tx.send(SessionEvent::Dead(server.to_string()));
            }
        })),
        server_requests: Some(req_tx),
        root: events.root.clone(),
    };
    let server_requests = Arc::new(Mutex::new(Some(req_rx)));
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
        ready_rx,
        server_requests,
    };

    tracing::info!(total = inner.entries.len(), "MCP servers connecting");
    tokio::spawn(run(
        inner, index, snapshot, cmd_rx, event_rx, ready_tx, wired,
    ));
    Some(handle)
}

/// One `ListChanged` per server per debounce window; a notification landing
/// inside the window defers a trailing emit instead of being dropped, so the
/// consumer eventually sees the burst's final state. Emits (immediate or
/// trailing) reset the window, bounding the rate under a continuous stream.
fn send_list_changed(
    tx: &mpsc::UnboundedSender<SessionEvent>,
    last_sent: &Arc<Mutex<HashMap<String, std::time::Instant>>>,
    server: &str,
) {
    let name = server.to_string();
    let mut sent = last_sent.lock().unwrap_or_else(|e| e.into_inner());
    let since = sent
        .get(&name)
        .map(|t| std::time::Instant::now().duration_since(*t));
    match since {
        Some(s) if s < REFRESH_DEBOUNCE => {
            let wait = REFRESH_DEBOUNCE - s;
            drop(sent);
            let tx = tx.clone();
            let last_sent = Arc::clone(last_sent);
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                send_list_changed(&tx, &last_sent, &name);
            });
        }
        _ => {
            sent.insert(name.clone(), std::time::Instant::now());
            drop(sent);
            let _ = tx.send(SessionEvent::ListChanged(name));
        }
    }
}

/// Connect results ride the same loop as commands, so a `Shutdown` arriving
/// mid-connect never waits for a slow `initialize`.
enum Step {
    Connected(usize, ConnectOutcome),
    Command(McpCommand),
    /// A session signaled a refresh/death through `SessionEvent`, or the
    /// debounce window closed and the queued refreshes fire.
    Session(SessionEvent),
    FlushRefreshes,
}

/// How a background connect/refresh task reports back.
pub(super) enum ConnectOutcome {
    Start(Result<StartResult, McpError>),
    /// Carries the session the refresh ran against, so a late result for an
    /// entry that was reconnected/toggled mid-refresh is dropped instead of
    /// clobbering the newer session's listings.
    Refresh {
        result: Result<StartResult, McpError>,
        session: Arc<dyn McpSession>,
    },
}

/// Server → loop signals from the session layer (list-changed notifications
/// and keepalive death).
pub(super) enum SessionEvent {
    ListChanged(String),
    Dead(String),
}

type ConnectedTx = mpsc::UnboundedSender<(usize, ConnectOutcome)>;

pub(super) async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    mut cmd_rx: mpsc::UnboundedReceiver<McpCommand>,
    mut event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    ready_tx: watch::Sender<bool>,
    events: McpEvents,
) {
    let (connected_tx, mut connected_rx): (_, mpsc::UnboundedReceiver<_>) =
        mpsc::unbounded_channel();
    // Held, not detached: dropping a pending connect aborts the task, which
    // drops the session it owns and kills the child process group it spawned.
    // The sender stays alive for the loop's lifetime so refreshes can also
    // ride this channel instead of blocking the command loop.
    let mut connects = spawn_connects(&inner, &connected_tx, &events);
    // In-flight `persist_enabled` writes from toggles; drained at shutdown.
    let mut persists = tokio::task::JoinSet::new();
    let mut ready_released = false;
    let mut ack: Option<tokio::sync::oneshot::Sender<()>> = None;
    // Debounce state for list-changed refreshes: servers pending a re-list,
    // and the (never-extended) deadline they flush at.
    let mut pending_refreshes: Vec<String> = Vec::new();
    let mut refresh_deadline: Option<tokio::time::Instant> = None;

    loop {
        release_ready(&inner, &ready_tx, &mut ready_released);
        let step = tokio::select! {
            result = connected_rx.recv() => match result {
                Some((i, outcome)) => Step::Connected(i, outcome),
                None => match cmd_rx.recv().await {
                    Some(cmd) => Step::Command(cmd),
                    None => break,
                },
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => Step::Command(cmd),
                None => break,
            },
            event = event_rx.recv() => match event {
                Some(event) => Step::Session(event),
                None => match cmd_rx.recv().await {
                    Some(cmd) => Step::Command(cmd),
                    None => break,
                },
            },
            // Fires when the debounce window closes; pending forever while
            // nothing is queued, so this arm never wins spuriously.
            _ = async {
                match refresh_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => Step::FlushRefreshes,
        };

        match step {
            Step::Connected(i, outcome) => {
                // A toggle or reconnect that ran mid-connect owns the entry
                // now, so a late result must not resurrect it. Dropping it
                // kills the session it carries.
                match outcome {
                    ConnectOutcome::Start(result)
                        if inner.entries[i].status == McpServerStatus::Connecting =>
                    {
                        let _ = apply_start_result(&mut inner.entries[i], result, "start");
                    }
                    ConnectOutcome::Start(_) => {}
                    ConnectOutcome::Refresh { result, session } => {
                        apply_refresh_result(&mut inner.entries[i], result, session).await;
                    }
                }
            }
            Step::Session(SessionEvent::ListChanged(server)) => {
                if !pending_refreshes.iter().any(|s| s == &server)
                    && inner
                        .entries
                        .iter()
                        .any(|e| e.name == server && e.session.is_some())
                {
                    pending_refreshes.push(server);
                    // First pending server opens the window; later ones share
                    // it, so the flush time stays bounded under a storm.
                    refresh_deadline.get_or_insert(tokio::time::Instant::now() + REFRESH_DEBOUNCE);
                }
            }
            Step::Session(SessionEvent::Dead(server)) => {
                if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server)
                    && entry.session.is_some()
                {
                    tracing::warn!(server = %server, "MCP session died (keepalive)");
                    entry.clear_connection().await;
                    entry.status = McpServerStatus::Failed("keepalive ping failed".into());
                }
            }
            Step::FlushRefreshes => {
                refresh_deadline = None;
                for server in std::mem::take(&mut pending_refreshes) {
                    handle_refresh(&mut inner, &server, &connected_tx);
                }
            }
            Step::Command(McpCommand::Toggle { server, enabled }) => {
                handle_toggle(
                    &mut inner,
                    &server,
                    enabled,
                    &connected_tx,
                    &mut connects,
                    &mut persists,
                    &events,
                )
                .await;
            }
            Step::Command(McpCommand::Reconnect { server }) => {
                handle_reconnect(&mut inner, &server, &connected_tx, &mut connects, &events).await;
            }
            Step::Command(McpCommand::Shutdown { ack: tx }) => {
                ack = Some(tx);
                break;
            }
        }
        inner.generation += 1;
        publish(&inner, &index, &snapshot);
    }
    drop(connects);
    drop(connected_tx);
    // Drop the session callbacks too: they hold clones of `event_tx`, and the
    // loop below no longer selects on `event_rx`.
    drop(events);
    drop(event_rx);
    // Await the tracked toggle persists before acknowledging shutdown; a
    // `JoinHandle` left to drop here would abandon an in-flight write and
    // silently lose the user's last toggle. Bounded, so a wedged filesystem
    // warns instead of stretching the caller's shutdown budget.
    let drain = async { while persists.join_next().await.is_some() {} };
    if tokio::time::timeout(PERSIST_DRAIN_TIMEOUT, drain)
        .await
        .is_err()
    {
        tracing::warn!("MCP toggle persistence still in flight at shutdown; state may be lost");
    }
    shutdown_all(&mut inner).await;
    inner.generation += 1;
    publish(&inner, &index, &snapshot);
    release_ready(&inner, &ready_tx, &mut ready_released);
    if let Some(tx) = ack {
        let _ = tx.send(());
    }
}

/// Re-list one server on its existing session (a `list_changed` notification
/// landed). Spawned like connects so a slow server never blocks the loop; the
/// result rides the same channel.
fn handle_refresh(inner: &mut McpManagerInner, server_name: &str, connected_tx: &ConnectedTx) {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        return;
    };
    let Some(session) = inner.entries[idx].session.clone() else {
        return;
    };
    tracing::info!(server = server_name, "MCP refresh (re-list) started");
    let tx = connected_tx.clone();
    tokio::spawn(async move {
        let result = session
            .refresh_listings()
            .await
            .map(|listings| StartResult {
                session: Arc::clone(&session),
                tool_infos: listings.tools,
                prompt_infos: listings.prompts,
                resource_infos: listings.resources,
            });
        let _ = tx.send((idx, ConnectOutcome::Refresh { result, session }));
    });
}

/// Apply a re-list result: swap in the new listings, or fail the entry when
/// the session can no longer answer. Late results for an entry whose session
/// was replaced mid-refresh (toggle/reconnect) are dropped by pointer
/// identity, the same way late `Start` results are dropped by status.
async fn apply_refresh_result(
    entry: &mut ServerEntry,
    result: Result<StartResult, McpError>,
    session: Arc<dyn McpSession>,
) {
    let still_current = entry
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session));
    if !still_current {
        tracing::info!(server = %entry.name, "dropping stale refresh result");
        return;
    }
    match result {
        Ok(start) => {
            entry.populate(start);
            tracing::info!(server = %entry.name, "MCP refresh (re-list) complete");
        }
        Err(e) => {
            tracing::warn!(server = %entry.name, error = %e, "MCP refresh failed");
            entry.clear_connection().await;
            entry.status = McpServerStatus::Failed(format!("refresh failed: {e}"));
        }
    }
}

/// Nothing left in `Connecting` means every server landed and the last publish
/// already carried its tools, so waiters can go.
fn release_ready(inner: &McpManagerInner, ready: &watch::Sender<bool>, released: &mut bool) {
    if *released
        || inner
            .entries
            .iter()
            .any(|e| e.status == McpServerStatus::Connecting)
    {
        return;
    }
    *released = true;
    let _ = ready.send(true);
    tracing::info!(
        running = inner.entries.iter().filter(|e| e.session.is_some()).count(),
        total = inner.entries.len(),
        "MCP servers initialized"
    );
}

pub(super) async fn handle_toggle(
    inner: &mut McpManagerInner,
    server_name: &str,
    enabled: bool,
    connected_tx: &ConnectedTx,
    connects: &mut tokio::task::JoinSet<()>,
    persists: &mut tokio::task::JoinSet<()>,
    events: &McpEvents,
) {
    if let Some(path) = inner
        .entries
        .iter()
        .find(|e| e.name == server_name)
        .map(|e| e.origin.clone())
    {
        spawn_persist_enabled(persists, path, server_name.to_owned(), enabled);
    }

    if enabled {
        begin_refresh(inner, server_name, connected_tx, connects, events).await;
    } else if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server_name) {
        entry.clear_connection().await;
        entry.status = McpServerStatus::Disabled;
    }

    tracing::info!(server = server_name, enabled, "MCP toggle complete");
}

/// Restart the server with its stored config.
async fn handle_reconnect(
    inner: &mut McpManagerInner,
    server_name: &str,
    connected_tx: &ConnectedTx,
    connects: &mut tokio::task::JoinSet<()>,
    events: &McpEvents,
) {
    let Some(entry) = inner.entries.iter().find(|e| e.name == server_name) else {
        tracing::warn!(server = server_name, "reconnect for unknown server");
        return;
    };
    if entry.status == McpServerStatus::Disabled {
        tracing::info!(
            server = server_name,
            "ignoring reconnect for disabled server"
        );
        return;
    }
    begin_refresh(inner, server_name, connected_tx, connects, events).await;
    tracing::info!(server = server_name, "MCP reconnect complete");
}

async fn shutdown_all(inner: &mut McpManagerInner) {
    for entry in &mut inner.entries {
        entry.clear_connection().await;
        if entry.status != McpServerStatus::Disabled {
            entry.status = McpServerStatus::Failed("shutdown".into());
        }
    }
    tracing::info!("MCP command loop shutting down");
}

/// Tear the old session down and wipe tools/prompts, mark the entry
/// `Connecting`, then reconnect in the background — through the same channel
/// the initial connects use, so no command ever awaits a server handshake
/// inline and a `Shutdown` arriving mid-refresh still preempts it. A failed
/// start leaves the entry empty instead of holding zombie tool references
/// into a dead session.
pub(super) async fn begin_refresh(
    inner: &mut McpManagerInner,
    server_name: &str,
    connected_tx: &ConnectedTx,
    connects: &mut tokio::task::JoinSet<()>,
    events: &McpEvents,
) {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        tracing::warn!(server = server_name, "refresh for unknown server");
        return;
    };
    let Some(config) = inner.entries[idx].config.clone() else {
        tracing::warn!(server = server_name, "refresh for server with no config");
        return;
    };
    {
        let entry = &mut inner.entries[idx];
        entry.status = McpServerStatus::Connecting;
        entry.clear_connection().await;
    }
    spawn_connect(idx, config, connected_tx, connects, events);
    tracing::info!(server = server_name, "MCP refresh started");
}

/// One background connect task reporting through `connected_tx` as it lands.
fn spawn_connect(
    idx: usize,
    config: ServerConfig,
    connected_tx: &ConnectedTx,
    set: &mut tokio::task::JoinSet<()>,
    events: &McpEvents,
) {
    let tx = connected_tx.clone();
    let events = events.clone();
    set.spawn(async move {
        let _ = tx.send((
            idx,
            ConnectOutcome::Start(connect_result(&config, &events).await),
        ));
    });
}

/// Connect and gather tool/prompt listings. Runs on a background task; every
/// await here is bounded by the server's configured timeout.
pub(super) async fn connect_result(
    config: &ServerConfig,
    events: &McpEvents,
) -> Result<StartResult, McpError> {
    let session = start_session(config, events.clone()).await?;
    let tool_infos = session.list_tools().await.unwrap_or_default();
    let prompt_infos = session.list_prompts().await.unwrap_or_default();
    let resource_infos = session.list_resources().await.unwrap_or_default();
    Ok(StartResult {
        session,
        tool_infos,
        prompt_infos,
        resource_infos,
    })
}

pub(super) fn status_from_err(e: &McpError) -> McpServerStatus {
    if let McpError::HttpError {
        status: 401,
        reason,
        ..
    } = e
    {
        McpServerStatus::NeedsAuth {
            url: Some(reason.clone()),
        }
    } else {
        McpServerStatus::Failed(e.to_string())
    }
}

/// The only place read-side state is updated. Every mutation in the command
/// loop ends here.
pub(super) fn publish(
    inner: &McpManagerInner,
    index: &ArcSwap<ToolIndex>,
    snapshot: &ArcSwap<McpSnapshot>,
) {
    let mut tools = HashMap::new();
    let mut prompts = HashMap::new();
    let mut resources = HashMap::new();
    let mut descriptors = Vec::new();
    let mut server_infos = Vec::with_capacity(inner.entries.len());
    let mut prompt_infos = Vec::new();
    let mut resource_infos = Vec::new();

    for entry in &inner.entries {
        let url = entry
            .config
            .as_ref()
            .and_then(|c| transport_url(&c.transport));
        let oauth = entry
            .config
            .as_ref()
            .and_then(|c| transport_oauth(&c.transport));

        if let Some(ref session) = entry.session
            && entry.status != McpServerStatus::Disabled
        {
            for t in &entry.tools {
                tools.insert(
                    Arc::clone(&t.qualified_name),
                    ToolRef {
                        raw_name: t.raw_name.clone(),
                        session: Arc::clone(session),
                    },
                );
                descriptors.push(McpToolDescriptor {
                    wire_name: wire_tool_name(&t.qualified_name),
                    qualified_name: t.qualified_name.to_string(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                    read_only_hint: t.read_only_hint,
                    destructive_hint: t.destructive_hint,
                });
            }
            for p in &entry.prompts {
                prompts.insert(
                    p.qualified_name.clone(),
                    PromptRef {
                        raw_name: p.raw_name.clone(),
                        session: Arc::clone(session),
                    },
                );
                prompt_infos.push(p.to_info(&entry.name));
            }
            for r in &entry.resources {
                resources.insert(
                    (entry.name.clone(), r.uri.clone()),
                    ResourceRef {
                        session: Arc::clone(session),
                    },
                );
                resource_infos.push(McpResourceInfo {
                    server: entry.name.clone(),
                    uri: r.uri.clone(),
                    name: r.name.clone(),
                    description: r.description.clone().unwrap_or_default(),
                    mime: r.mime.clone(),
                    size: r.size,
                });
            }
        }

        server_infos.push(McpServerInfo {
            name: entry.name.clone(),
            transport_kind: entry.transport_kind,
            tool_count: entry.tools.len(),
            prompt_count: entry.prompts.len(),
            resource_count: entry.resources.len(),
            status: entry.status.clone(),
            config_path: entry.origin.clone(),
            url,
            oauth,
        });
    }

    index.store(Arc::new(ToolIndex {
        tools,
        prompts,
        resources,
        descriptors,
    }));
    snapshot.store(Arc::new(McpSnapshot {
        infos: server_infos,
        prompts: prompt_infos,
        resources: resource_infos,
        generation: inner.generation,
    }));
}

fn transport_url(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { .. } => None,
    }
}

fn transport_oauth(transport: &Transport) -> Option<OauthClientConfig> {
    match transport {
        Transport::Http { oauth, .. } => oauth.clone(),
        Transport::Stdio { .. } => None,
    }
}

fn parse_entries(config: McpConfig) -> McpManagerInner {
    let origins = config.origins;
    let mut entries = Vec::with_capacity(config.mcp.len());

    for (name, raw) in config.mcp {
        let transport_kind = transport_kind(&raw.transport);
        let origin = origins.get(&name).cloned().unwrap_or_default();
        let disabled = !raw.enabled;
        let (config, status) = match parse_server(name.clone(), raw) {
            Ok(sc) if disabled => (Some(sc), McpServerStatus::Disabled),
            Ok(sc) => (Some(sc), McpServerStatus::Connecting),
            Err(e) => {
                tracing::warn!(server = %name, error = %e, "invalid MCP server config");
                (None, McpServerStatus::Failed(e.to_string()))
            }
        };
        entries.push(ServerEntry {
            name,
            config,
            transport_kind,
            origin,
            status,
            session: None,
            tools: Vec::new(),
            prompts: Vec::new(),
            resources: Vec::new(),
        });
    }

    McpManagerInner {
        entries,
        generation: 0,
    }
}

/// One task per enabled server, each reporting back as it lands, so `run`
/// publishes a fast server's tools without waiting for the slowest.
fn spawn_connects(
    inner: &McpManagerInner,
    tx: &ConnectedTx,
    events: &McpEvents,
) -> tokio::task::JoinSet<()> {
    let mut set = tokio::task::JoinSet::new();
    for (i, config) in inner
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == McpServerStatus::Connecting)
        .filter_map(|(i, e)| e.config.clone().map(|c| (i, c)))
    {
        spawn_connect(i, config, tx, &mut set, events);
    }
    set
}

pub(super) fn apply_start_result(
    entry: &mut ServerEntry,
    result: Result<StartResult, McpError>,
    action: &'static str,
) -> Result<(), McpError> {
    match result {
        Ok(start) => {
            entry.populate(start);
            Ok(())
        }
        Err(e) => {
            entry.status = status_from_err(&e);
            if !matches!(entry.status, McpServerStatus::NeedsAuth { .. }) {
                tracing::warn!(server = %entry.name, action, error = %e, "MCP server start failed");
            }
            Err(e)
        }
    }
}

/// Persist a toggle in the background. The task is *tracked*, not detached:
/// `run` keeps the `JoinSet` and drains it on shutdown, so a toggled enabled
/// flag cannot be lost because a dropped `JoinHandle` abandoned the write.
fn spawn_persist_enabled(
    persists: &mut tokio::task::JoinSet<()>,
    path: PathBuf,
    name: String,
    enabled: bool,
) {
    let log_name = name.clone();
    persists.spawn(async move {
        if let Err(e) =
            tokio::task::spawn_blocking(move || config::persist_enabled(&path, &name, enabled))
                .await
                .unwrap_or_else(|e| {
                    Err(McpError::Config {
                        message: format!("spawn_blocking failed: {e}"),
                    })
                })
        {
            tracing::warn!(error = %e, server = %log_name, "failed to persist MCP toggle");
        }
    });
}
