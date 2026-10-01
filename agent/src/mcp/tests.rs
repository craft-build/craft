use super::manager::*;
use super::session::BoxFuture;
use super::session::start_session;
use super::test_support::stub_handle;
use super::*;
use config::{McpConfig, RawHttpFields, RawServerConfig, RawStdioFields, RawTransport, Transport};
use std::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MISSING_PROGRAM: &str = "/nonexistent/definitely-not-here";

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

fn make_config(entries: Vec<(&str, RawServerConfig)>) -> McpConfig {
    let mut mcp = HashMap::new();
    let mut origins = HashMap::new();
    for (name, cfg) in entries {
        origins.insert(name.to_string(), PathBuf::from("/test/config.toml"));
        mcp.insert(name.to_string(), cfg);
    }
    McpConfig { mcp, origins }
}

const TOOL_NAME: &str = "srv.tool";
const WIRE_TOOL_NAME: &str = "srv__tool";

#[test]
fn wire_name_round_trip() {
    assert_eq!(wire_tool_name("srv.echo_tool"), "srv__echo_tool");
    assert_eq!(internal_tool_name("srv__echo_tool"), "srv.echo_tool");
    // Only the first `__` is the separator.
    assert_eq!(internal_tool_name("srv__deep__name"), "srv.deep__name");
}

/// Counts shutdowns, signals on `call_entered` the moment a `tools/call`
/// begins, and holds the call inside `call_gate` until tests release it.
struct FakeSession {
    name: Arc<str>,
    shutdowns: AtomicUsize,
    call_entered: tokio::sync::mpsc::UnboundedSender<()>,
    call_gate: tokio::sync::Mutex<()>,
}

impl FakeSession {
    fn new() -> Arc<Self> {
        let (call_entered, _) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self {
            name: Arc::from("fake"),
            shutdowns: AtomicUsize::new(0),
            call_entered,
            call_gate: tokio::sync::Mutex::new(()),
        })
    }

    fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
}

impl McpSession for FakeSession {
    fn server_name(&self) -> &str {
        &self.name
    }
    fn read_resource<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<String, McpError>> {
        Box::pin(async move { Ok(format!("[fake read of {uri}]")) })
    }
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn call_tool(
        &self,
        _name: &str,
        _args: &Value,
    ) -> BoxFuture<'_, Result<McpToolOutput, McpError>> {
        Box::pin(async {
            let _ = self.call_entered.send(());
            let _g = self.call_gate.lock().await;
            Ok(McpToolOutput {
                parts: vec![session::McpPart::Text("ok".into())],
            })
        })
    }
    fn call_tool_cancellable<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a crate::run::CancelToken,
    ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
        super::session::race_call_cancel(self, name, args, cancel)
    }
    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_prompt(
        &self,
        _name: &str,
        _arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<session::PromptMessage>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// [`FakeSession`] whose `refresh_listings` counts calls and returns a
/// different tool set than the connect-time cache, to prove the manager
/// really re-lists (not just republishes) on `list_changed`.
struct RefreshableSession {
    name: Arc<str>,
    refreshes: AtomicUsize,
}

fn resource_info(uri: &str, name: &str) -> ResourceInfo {
    ResourceInfo {
        uri: uri.into(),
        name: name.into(),
        description: Some("a resource".into()),
        mime: Some("text/plain".into()),
        size: Some(42),
    }
}

fn tool_info(name: &str) -> ToolInfo {
    ToolInfo {
        name: name.into(),
        description: String::new(),
        input_schema: serde_json::json!({}),
        read_only_hint: None,
        destructive_hint: None,
    }
}

impl McpSession for RefreshableSession {
    fn server_name(&self) -> &str {
        &self.name
    }
    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<ResourceInfo>, McpError>> {
        Box::pin(async { Ok(vec![resource_info("mem:///one", "one")]) })
    }
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
        Box::pin(async { Ok(vec![tool_info("tool")]) })
    }
    fn call_tool(
        &self,
        _name: &str,
        _args: &Value,
    ) -> BoxFuture<'_, Result<McpToolOutput, McpError>> {
        Box::pin(async {
            Ok(McpToolOutput {
                parts: vec![session::McpPart::Text("ok".into())],
            })
        })
    }
    fn call_tool_cancellable<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a crate::run::CancelToken,
    ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
        super::session::race_call_cancel(self, name, args, cancel)
    }
    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_prompt(
        &self,
        _name: &str,
        _arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<session::PromptMessage>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn refresh_listings(&self) -> BoxFuture<'_, Result<session::Listings, McpError>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(session::Listings {
                tools: vec![tool_info("tool"), tool_info("tool2")],
                prompts: Vec::new(),
                resources: vec![
                    resource_info("mem:///one", "one"),
                    resource_info("mem:///two", "two"),
                ],
            })
        })
    }
}

/// Drive the real `run` loop against one RefreshableSession entry,
/// returning the handle and the channel that session notifications use.
fn spawn_loop_with_session(
    session: Arc<RefreshableSession>,
) -> (McpHandle, mpsc::UnboundedSender<SessionEvent>) {
    let inner = McpManagerInner {
        entries: vec![fake_entry("srv", session as _)],
        generation: 0,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    publish(&inner, &index, &snapshot);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = watch::channel(false);
    let _ = ready_tx.send(true);
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
        ready_rx,
        server_requests: Arc::new(Mutex::new(None)),
    };
    tokio::spawn(run(
        inner,
        index,
        snapshot,
        cmd_rx,
        event_rx,
        ready_tx,
        McpEvents::default(),
    ));
    (handle, event_tx)
}

/// A `list_changed` notification re-lists the server on its existing
/// session and republishes the snapshot with the new tools.
#[tokio::test]
async fn notification_refresh_republishes_new_tools() {
    let session = Arc::new(RefreshableSession {
        name: Arc::from("srv"),
        refreshes: AtomicUsize::new(0),
    });
    let (handle, event_tx) = spawn_loop_with_session(Arc::clone(&session));
    assert_eq!(handle.reader().load().infos[0].tool_count, 1);

    event_tx
        .send(SessionEvent::ListChanged("srv".into()))
        .unwrap();
    for _ in 0..100 {
        if handle.reader().load().infos[0].tool_count == 2 {
            assert_eq!(session.refreshes.load(Ordering::SeqCst), 1);
            assert!(handle.has_tool("srv.tool2"));
            handle.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("refresh never republished the new tool set");
}

/// A burst of notifications inside one debounce window coalesces into a
/// single re-list.
#[tokio::test]
async fn notification_bursts_are_debounced() {
    let session = Arc::new(RefreshableSession {
        name: Arc::from("srv"),
        refreshes: AtomicUsize::new(0),
    });
    let (handle, event_tx) = spawn_loop_with_session(Arc::clone(&session));

    for _ in 0..20 {
        event_tx
            .send(SessionEvent::ListChanged("srv".into()))
            .unwrap();
    }
    // Well past the debounce window plus one refresh round-trip.
    tokio::time::sleep(REFRESH_DEBOUNCE * 4).await;
    assert_eq!(
        session.refreshes.load(Ordering::SeqCst),
        1,
        "burst must coalesce into one refresh"
    );
    assert_eq!(handle.reader().load().infos[0].tool_count, 2);
    handle.shutdown().await;
}

/// Phase 5: resources flow `populate` → `publish` → snapshot (with
/// `resource_count`), and `McpHandle::read_resource` routes the
/// (server, uri) pair to the entry's session — unknown pairs refuse.
#[tokio::test]
async fn resources_flow_into_snapshot_and_reads_route() {
    let t = FakeSession::new();
    let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
    inner.entries[0].populate(StartResult {
        session: t,
        tool_infos: Vec::new(),
        prompt_infos: Vec::new(),
        resource_infos: vec![resource_info("file:///notes.txt", "notes")],
    });
    inner.generation += 1;
    publish(&inner, &handle.index, &handle.snapshot);

    assert_eq!(handle.reader().load().infos[0].resource_count, 1);
    let resources = handle.reader().load().resources.clone();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].server, "srv");
    assert_eq!(resources[0].uri, "file:///notes.txt");
    assert_eq!(resources[0].name, "notes");
    assert!(handle.has_resources());

    let text = handle
        .read_resource("srv", "file:///notes.txt")
        .await
        .unwrap();
    assert_eq!(text, "[fake read of file:///notes.txt]");
    assert!(
        handle
            .read_resource("srv", "file:///missing")
            .await
            .is_err()
    );
    assert!(
        handle
            .read_resource("other", "file:///notes.txt")
            .await
            .is_err()
    );
}

/// A `resources/list_changed` (same refresh path) re-lists resources
/// alongside tools and republishes the snapshot.
#[tokio::test]
async fn notification_refresh_republishes_new_resources() {
    let session = Arc::new(RefreshableSession {
        name: Arc::from("srv"),
        refreshes: AtomicUsize::new(0),
    });
    let (handle, event_tx) = spawn_loop_with_session(Arc::clone(&session));
    assert_eq!(handle.reader().load().infos[0].resource_count, 0);

    event_tx
        .send(SessionEvent::ListChanged("srv".into()))
        .unwrap();
    for _ in 0..100 {
        if handle.reader().load().infos[0].resource_count == 2 {
            let resources = handle.reader().load().resources.clone();
            assert_eq!(resources.len(), 2);
            assert!(resources.iter().any(|r| r.uri == "mem:///two"));
            handle.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("refresh never republished the new resource set");
}

/// A keepalive death signal fails the entry so the user sees Reconnect.
#[tokio::test]
async fn keepalive_death_marks_the_entry_failed() {
    let session = Arc::new(RefreshableSession {
        name: Arc::from("srv"),
        refreshes: AtomicUsize::new(0),
    });
    let (handle, event_tx) = spawn_loop_with_session(session);
    event_tx.send(SessionEvent::Dead("srv".into())).unwrap();
    for _ in 0..100 {
        let status = handle.reader().load().infos[0].status.clone();
        if matches!(status, McpServerStatus::Failed(_)) {
            assert!(!handle.has_tool(TOOL_NAME));
            handle.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("death signal never failed the entry");
}

fn fake_entry(name: &str, session: Arc<dyn McpSession>) -> ServerEntry {
    let qualified = intern(format!("{name}{SEPARATOR}tool"));
    ServerEntry {
        name: name.into(),
        config: None,
        transport_kind: "fake",
        origin: PathBuf::new(),
        status: McpServerStatus::Running,
        session: Some(session),
        tools: vec![McpToolDef {
            qualified_name: qualified,
            raw_name: "tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            read_only_hint: None,
            destructive_hint: None,
        }],
        prompts: Vec::new(),
        resources: Vec::new(),
    }
}

fn bad_stdio_config(name: &str) -> ServerConfig {
    ServerConfig {
        name: name.into(),
        timeout: Duration::from_secs(1),
        transport: Transport::Stdio {
            program: MISSING_PROGRAM.into(),
            args: vec![],
            environment: HashMap::new(),
        },
    }
}

/// Build `inner`, publish it into fresh `ArcSwap`s, and return a live
/// `McpHandle` pointing at the same state so tests can hit both the
/// mutation and the read path.
fn setup(entries: Vec<ServerEntry>) -> (McpManagerInner, McpHandle) {
    let inner = McpManagerInner {
        entries,
        generation: 0,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    publish(&inner, &index, &snapshot);
    let handle = McpHandle {
        cmd_tx: mpsc::unbounded_channel().0,
        index,
        snapshot,
        ready_rx: watch::channel(true).1,
        server_requests: Arc::new(Mutex::new(None)),
    };
    (inner, handle)
}

/// `ready` carries the correctness of connecting in the background: a
/// prompt typed during startup must not ship before the servers settle.
#[tokio::test]
async fn ready_settles_every_server_status() {
    assert!(start_with_config(McpConfig::default()).is_none());

    let mut disabled = stdio_raw(&["unused-disabled-cmd"]);
    disabled.enabled = false;
    let mut http_missing = RawServerConfig {
        enabled: true,
        timeout: DEFAULT_TIMEOUT_MS,
        transport: RawTransport::Http(RawHttpFields {
            url: "https://invalid.invalid/mcp".into(),
            headers: HashMap::new(),
            oauth: None,
        }),
    };
    http_missing.timeout = 500;
    let config = make_config(vec![
        ("disabled-srv", disabled),
        ("unparseable-srv", stdio_raw(&[])),
        ("unspawnable-srv", stdio_raw(&[MISSING_PROGRAM])),
    ]);
    let handle = start_with_config(config).unwrap();
    handle.ready().await;

    let infos = handle.reader().load().infos.clone();
    let status = |name: &str| {
        &infos
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("{name} must be published"))
            .status
    };
    let failed = |name: &str| matches!(status(name), McpServerStatus::Failed(_));
    assert!(failed("unparseable-srv"));
    assert!(failed("unspawnable-srv"));
    assert_eq!(*status("disabled-srv"), McpServerStatus::Disabled);
}

/// `sleep` spawns fine and never answers `initialize`, so its connect only
/// ends on the request timeout, far past the shutdown one. Shutdown has to
/// preempt it, or quitting during startup hangs.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_preempts_an_in_flight_connect() {
    const BLOCKED: &str = "shutdown must not wait for an in-flight connect";
    let config = make_config(vec![("slow-srv", stdio_raw(&["sleep", "60"]))]);
    let handle = start_with_config(config).unwrap();
    let started = std::time::Instant::now();
    handle.shutdown().await;
    assert!(started.elapsed() < MCP_SHUTDOWN_TIMEOUT, "{BLOCKED}");
}

/// If a refresh fails, the entry must end up empty. A zombie tool left
/// behind would be handed to the model on the next turn and then try to
/// call into a dead session. Exercises the same pieces the background
/// refresh path composes: clear + Connecting, connect, apply.
#[tokio::test]
async fn failed_refresh_clears_entry() {
    let t = FakeSession::new();
    let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
    let bad_config = bad_stdio_config("srv");
    inner.entries[0].config = Some(bad_config);

    {
        let entry = &mut inner.entries[0];
        entry.status = McpServerStatus::Connecting;
        entry.clear_connection().await;
    }
    let result = connect_result(
        inner.entries[0].config.as_ref().unwrap(),
        &McpEvents::default(),
    )
    .await;
    assert!(result.is_err());
    apply_start_result(&mut inner.entries[0], result, "refresh").unwrap_err();

    let entry = &inner.entries[0];
    assert_eq!(t.shutdowns(), 1);
    assert!(entry.tools.is_empty());
    assert!(entry.prompts.is_empty());
    assert!(entry.session.is_none());
    assert!(matches!(entry.status, McpServerStatus::Failed(_)));
}

/// A refresh must not run inline: while one is in flight, a Shutdown must
/// still preempt it (same guarantee the initial connects give).
#[tokio::test]
async fn shutdown_preempts_an_in_flight_refresh() {
    let t = FakeSession::new();
    let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
    inner.entries[0].config = Some(ServerConfig {
        name: "srv".into(),
        timeout: Duration::from_secs(30),
        transport: Transport::Stdio {
            program: "sleep".into(),
            args: vec!["60".into()],
            environment: HashMap::new(),
        },
    });

    let (tx, rx) = mpsc::unbounded_channel();
    let mut connects = tokio::task::JoinSet::new();
    let started = std::time::Instant::now();
    begin_refresh(&mut inner, "srv", &tx, &mut connects, &McpEvents::default()).await;
    drop(tx);
    drop(connects);
    drop(rx);
    assert!(
        started.elapsed() < MCP_SHUTDOWN_TIMEOUT,
        "refresh start must not await the handshake"
    );
    assert_eq!(inner.entries[0].status, McpServerStatus::Connecting);
}

#[tokio::test]
async fn disable_purges_entry_and_published_view() {
    let t = FakeSession::new();
    let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

    assert!(handle.has_tool(TOOL_NAME));
    let descriptors = handle.tool_descriptors();
    assert_eq!(descriptors[0].wire_name, WIRE_TOOL_NAME);

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut connects = tokio::task::JoinSet::new();
    let mut persists = tokio::task::JoinSet::new();
    handle_toggle(
        &mut inner,
        "srv",
        false,
        &tx,
        &mut connects,
        &mut persists,
        &McpEvents::default(),
    )
    .await;
    publish(&inner, &handle.index, &handle.snapshot);

    let entry = &inner.entries[0];
    assert_eq!(t.shutdowns(), 1);
    assert!(entry.tools.is_empty());
    assert!(entry.session.is_none());
    assert_eq!(entry.status, McpServerStatus::Disabled);
    assert!(!handle.has_tool(TOOL_NAME));
    assert!(handle.tool_descriptors().is_empty());
}

/// Regression (reference semantics): a slow tool call must not block a
/// publish. The call is parked on `call_gate` while `publish` runs.
#[tokio::test]
async fn slow_tool_call_does_not_block_publish() {
    let t = FakeSession::new();
    let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

    let held = t.call_gate.lock().await;
    let call_handle = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .call_tool(TOOL_NAME, &serde_json::json!({}))
                .await
                .unwrap()
        })
    };

    // Give the call a moment to park inside the session.
    tokio::time::sleep(Duration::from_millis(50)).await;
    inner.generation += 1;
    publish(&inner, &handle.index, &handle.snapshot);
    assert_eq!(handle.snapshot.load().generation, 1);

    drop(held);
    call_handle.await.unwrap();
}

/// Commands must actually be awaited in the loop: a toggle-enable on a
/// disabled server drives it Connecting → Failed through the real
/// command loop (regression: a refactor once constructed the handler
/// future without polling it, so commands silently did nothing).
#[tokio::test]
async fn toggle_enable_runs_a_background_refresh() {
    let mut raw = stdio_raw(&[MISSING_PROGRAM]);
    raw.timeout = 1_000;
    raw.enabled = false;
    let config = make_config(vec![("ghost", raw)]);
    let handle = start_with_config(config).unwrap();
    handle.ready().await;
    assert_eq!(
        handle.reader().load().infos[0].status,
        McpServerStatus::Disabled
    );

    handle.send(McpCommand::Toggle {
        server: "ghost".into(),
        enabled: true,
    });
    for _ in 0..200 {
        let status = handle.reader().load().infos[0].status.clone();
        if matches!(status, McpServerStatus::Failed(_)) {
            handle.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("toggle-enable never reached Failed");
}

#[tokio::test]
async fn shutdown_command_drains_and_acks() {
    let (t1, t2) = (FakeSession::new(), FakeSession::new());
    let inner = McpManagerInner {
        entries: vec![
            fake_entry("a", Arc::clone(&t1) as _),
            fake_entry("b", Arc::clone(&t2) as _),
        ],
        generation: 0,
    };
    let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    let loop_task = tokio::spawn(run(
        inner,
        Arc::clone(&index),
        Arc::clone(&snapshot),
        cmd_rx,
        event_rx,
        watch::channel(false).0,
        McpEvents::default(),
    ));

    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    cmd_tx.send(McpCommand::Shutdown { ack: ack_tx }).unwrap();
    ack_rx.await.unwrap();
    loop_task.await.unwrap();

    assert_eq!(t1.shutdowns(), 1);
    assert_eq!(t2.shutdowns(), 1);
    assert!(snapshot.load().infos.iter().all(|i| i.tool_count == 0));
}

#[test]
fn overlong_wire_names_are_filtered_at_populate() {
    let session: Arc<dyn McpSession> = FakeSession::new();
    let mut entry = ServerEntry {
        name: "averyveryverylongservername".into(),
        config: None,
        transport_kind: "fake",
        origin: PathBuf::new(),
        status: McpServerStatus::Connecting,
        session: None,
        tools: Vec::new(),
        prompts: Vec::new(),
        resources: Vec::new(),
    };
    let long_tool = "x".repeat(60);
    entry.populate(StartResult {
        session,
        tool_infos: vec![ToolInfo {
            name: long_tool,
            description: String::new(),
            input_schema: serde_json::json!({}),
            read_only_hint: None,
            destructive_hint: None,
        }],
        prompt_infos: Vec::new(),
        resource_infos: Vec::new(),
    });
    assert!(entry.tools.is_empty(), "64-char wire limit must filter");
}

#[test]
fn stub_handle_publishes_wire_names() {
    let handle = stub_handle(&[("srv.tool", "a tool")]);
    assert!(handle.has_tool("srv.tool"));
    let descriptors = handle.tool_descriptors();
    assert_eq!(descriptors[0].wire_name, "srv__tool");
    assert_eq!(descriptors[0].parameters["type"], "object");
}

/// Phase 3: server-declared annotations flow ToolInfo → `populate` →
/// `publish` → descriptors, and the permission engine can sync them from
/// the published generation without re-registering per turn.
#[tokio::test]
async fn annotations_flow_to_descriptors_and_permission_sync() {
    let t = FakeSession::new();
    let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
    // The initial generation-0 snapshot still populates the table.
    let gen0 = crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir().into(),
    );
    gen0.sync_mcp_annotations(&handle);
    inner.entries[0].populate(StartResult {
        session: t,
        tool_infos: vec![
            ToolInfo {
                name: "get".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                read_only_hint: Some(true),
                destructive_hint: Some(false),
            },
            ToolInfo {
                name: "nuke".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                read_only_hint: Some(false),
                destructive_hint: Some(true),
            },
            ToolInfo {
                name: "plain".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                read_only_hint: None,
                destructive_hint: None,
            },
        ],
        prompt_infos: Vec::new(),
        resource_infos: Vec::new(),
    });
    inner.generation += 1;
    publish(&inner, &handle.index, &handle.snapshot);

    let by_name = |n: &str| {
        handle
            .tool_descriptors()
            .into_iter()
            .find(|d| d.qualified_name == format!("srv.{n}"))
            .unwrap_or_else(|| panic!("missing descriptor for {n}"))
    };
    assert_eq!(by_name("get").read_only_hint, Some(true));
    assert_eq!(by_name("get").destructive_hint, Some(false));
    assert_eq!(by_name("nuke").destructive_hint, Some(true));
    assert_eq!(by_name("plain").read_only_hint, None);

    // Sync is generation-keyed: the second call is a no-op, and the
    // manager still surfaces the hints to a permission check.
    // First sync happens at the initial generation-0 snapshot.
    let mgr = crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir().into(),
    );
    mgr.sync_mcp_annotations(&handle);
    mgr.sync_mcp_annotations(&handle);
    let tool = crate::permissions::ToolKey::McpTool {
        server: "srv".into(),
        tool: "get".into(),
    };
    match mgr.check(&tool, &["{}".to_string()]) {
        crate::permissions::PermissionCheck::NeedsPrompt { low_risk, .. } => {
            assert!(low_risk);
        }
        other => panic!("expected NeedsPrompt, got {other:?}"),
    }
}

#[test]
fn http_401_maps_to_needs_auth() {
    let status = status_from_err(&McpError::HttpError {
        server: "srv".into(),
        status: 401,
        reason: "Bearer realm=...".into(),
    });
    assert!(matches!(
        status,
        McpServerStatus::NeedsAuth { url: Some(ref u) } if u.contains("realm")
    ));
    let other = status_from_err(&McpError::Timeout {
        server: "srv".into(),
        timeout_ms: 10,
    });
    assert!(matches!(other, McpServerStatus::Failed(_)));
}

/// End-to-end over stdio against a minimal in-process MCP server (python3
/// speaking newline-delimited JSON-RPC): connect, list, call, timeout path.
#[cfg(unix)]
#[tokio::test]
async fn stdio_end_to_end_against_a_mock_server() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // no python3 on this host; skip rather than fail
    }
    const SCRIPT: &str = r#"
import json, sys, time
listed = False
def send(msg): sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method, rid = req.get("method"), req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{},"resources":{}},"serverInfo":{"name":"mock","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        if not listed:
            listed = True
            send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo","description":"Echo a message","inputSchema":{"type":"object","properties":{"message":{"type":"string"}}}}]}})
            # Announce a change right after the first listing so the client's
            # list_changed path is exercised; the next tools/list answers with
            # the new tool set.
            send({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
        else:
            send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo2","description":"Echo, revised","inputSchema":{"type":"object"}}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"prompts":[{"name":"greet","description":"Greet","arguments":[{"name":"who","required":True}]}]}})
    elif method == "resources/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"resources":[
            {"uri":"file:///notes.txt","name":"notes","description":"Project notes","mimeType":"text/plain","size":6},
            {"uri":"db://users","name":"users","mimeType":"application/octet-stream","size":3}]}})
    elif method == "resources/read":
        uri = req["params"]["uri"]
        if uri == "file:///notes.txt":
            send({"jsonrpc":"2.0","id":rid,"result":{"contents":[{"uri":uri,"mimeType":"text/plain","text":"hello"}]}})
        elif uri == "db://users":
            send({"jsonrpc":"2.0","id":rid,"result":{"contents":[{"uri":uri,"mimeType":"application/octet-stream","blob":"QUJD"}]}})
        else:
            send({"jsonrpc":"2.0","id":rid,"error":{"code":-32602,"message":"unknown resource"}})
    elif method == "tools/call":
        name = req["params"]["name"]
        if name == "echo":
            msg = req["params"].get("arguments", {}).get("message", "")
            send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":f"echo: {msg}"}],"isError":False}})
        elif name == "boom":
            send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":"exploded"}],"isError":True}})
        elif name == "hang":
            time.sleep(30)
        else:
            send({"jsonrpc":"2.0","id":rid,"error":{"code":-32602,"message":"unknown tool"}})
    elif method == "prompts/get":
        send({"jsonrpc":"2.0","id":rid,"result":{"messages":[{"role":"user","content":{"type":"text","text":"hello prompt"}}]}})
"#;
    let config = ServerConfig {
        name: "mock".into(),
        timeout: Duration::from_secs(5),
        transport: Transport::Stdio {
            program: "python3".into(),
            args: vec!["-u".into(), "-c".into(), SCRIPT.into()],
            environment: HashMap::new(),
        },
    };
    // The mock announces tools/list_changed right after the first
    // listing; prove the client handler fired and the follow-up re-list
    // picked up the new tool set.
    let (cb_tx, mut cb_rx) = tokio::sync::mpsc::unbounded_channel();
    let events = McpEvents {
        on_list_changed: Some(Arc::new(move |server| {
            let _ = cb_tx.send(server.to_string());
        })),
        ..McpEvents::default()
    };
    let session = start_session(&config, events)
        .await
        .expect("connect mock server");

    let tools = session.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    // The notification arrives asynchronously; wait for the callback,
    // then re-list and confirm the session's cache picked up `echo2`.
    let notified = tokio::time::timeout(Duration::from_secs(3), cb_rx.recv()).await;
    assert_eq!(
        notified.expect("list_changed callback fired").as_deref(),
        Some("mock")
    );
    let listings = session.refresh_listings().await.unwrap();
    let tools = listings.tools;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo2");
    assert_eq!(session.list_tools().await.unwrap()[0].name, "echo2");

    let text = session
        .call_tool("echo", &serde_json::json!({"message": "hi"}))
        .await
        .unwrap()
        .joined_text();
    assert_eq!(text, "echo: hi");

    // `isError: true` surfaces as an error carrying the tool's text.
    let err = session
        .call_tool("boom", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("exploded"), "got: {err}");

    // Prompts flow through the same session.
    let prompts = session.list_prompts().await.unwrap();
    assert_eq!(prompts.len(), 1);
    let messages = session.get_prompt("greet", &HashMap::new()).await.unwrap();
    assert_eq!(messages[0].text.as_deref(), Some("hello prompt"));

    // Phase 5: resources list and read (text inline, blob under a mime
    // header line).
    let resources = session.list_resources().await.unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].name, "notes");
    assert_eq!(resources[0].size, Some(6));
    assert_eq!(
        session.read_resource("file:///notes.txt").await.unwrap(),
        "hello"
    );
    let blob = session.read_resource("db://users").await.unwrap();
    assert!(blob.contains("[base64 blob resource db://users"), "{blob}");
    assert!(blob.contains("QUJD"), "{blob}");
    let err = session.read_resource("file:///missing").await.unwrap_err();
    assert!(err.to_string().contains("unknown resource"), "got: {err}");

    // A hanging tool call must hit the per-request timeout, not hang forever.
    let start = std::time::Instant::now();
    let err = session
        .call_tool("hang", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Timeout { .. }), "got: {err}");
    assert!(start.elapsed() < Duration::from_secs(10));

    session.shutdown().await;
}

/// End-to-end: a tool returning an image block plus `structuredContent`
/// keeps both — the image as a structured part with its mime, the JSON as
/// a fenced block in the text. Non-text prompt messages degrade to text
/// (`[image omitted]`) instead of vanishing.
#[cfg(unix)]
#[tokio::test]
async fn stdio_e2e_preserves_image_and_structured_content() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // no python3 on this host; skip rather than fail
    }
    const SCRIPT: &str = r#"
import json, sys
def send(msg): sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method, rid = req.get("method"), req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{}},"serverInfo":{"name":"mock2","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"rich","description":"Rich output","inputSchema":{"type":"object"}}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"prompts":[]}})
    elif method == "tools/call":
        send({"jsonrpc":"2.0","id":rid,"result":{
            "content":[
                {"type":"text","text":"chart ready"},
                {"type":"image","data":"aWNvbg==","mimeType":"image/png"}
            ],
            "structuredContent":{"series":[1,2,3]},
            "isError":False}})
    elif method == "prompts/get":
        send({"jsonrpc":"2.0","id":rid,"result":{"messages":[
            {"role":"user","content":{"type":"text","text":"look at this"}},
            {"role":"user","content":{"type":"image","data":"aWNvbg==","mimeType":"image/png"}}]}})
"#;
    let config = ServerConfig {
        name: "mock2".into(),
        timeout: Duration::from_secs(5),
        transport: Transport::Stdio {
            program: "python3".into(),
            args: vec!["-u".into(), "-c".into(), SCRIPT.into()],
            environment: HashMap::new(),
        },
    };
    let session = start_session(&config, McpEvents::default())
        .await
        .expect("connect mock server");

    let output = session
        .call_tool("rich", &serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(output.parts.len(), 3, "text, image, json");
    assert!(matches!(&output.parts[0], session::McpPart::Text(t) if t.contains("chart ready")));
    assert!(
        matches!(&output.parts[1], session::McpPart::Image(i) if i.data == "aWNvbg==" && i.mime == "image/png")
    );
    assert!(
        matches!(&output.parts[2], session::McpPart::Text(t) if t.contains("```json") && t.contains("\"series\"")),
        "structuredContent must land as a fenced JSON block after the blocks"
    );
    assert!(output.joined_text().contains("chart ready"));

    let messages = session.get_prompt("any", &HashMap::new()).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text.as_deref(), Some("look at this"));
    assert_eq!(messages[1].text.as_deref(), Some("[image omitted]"));

    session.shutdown().await;
}
