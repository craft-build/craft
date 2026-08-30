use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use craft_agent::permissions::is_universal_scope;
use craft_agent::prompt::{PromptId, Slot, SlotKind, ValidNames};
use craft_agent::tools::Tool;
use craft_agent::tools::registry::ToolRegistry;
use craft_agent::tools::schema::{ParamSchema, to_json_schema, try_from_json, validate};
use craft_agent::tools::{
    BoxFuture, Deadline, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
    PermissionScopes, ToolAudience, ToolContext, ToolExecResult, ToolInvocation,
};
use craft_agent::{AgentEvent, BufferSnapshot, ImageMediaType, ImageSource, SharedBuf, ToolOutput};
use craft_config::{Effect, PermissionRule, ToolKey};
use flume::Sender;
use mlua::{
    Function, Lua, LuaSerdeExt, RegistryKey, Result as LuaResult, Table, Value as LuaValue,
};
use serde_json::Value;

use crate::api::ui::buf::BufHandle;
use crate::api::util::command::{
    CommandEntry, CommandHandlerMap, LuaCommandWriter, UiAction, publish_command_snapshot,
    ui_roundtrip,
};
use crate::api::util::ctx::LuaCtx;
use crate::api::util::pair::{Pair, try_pair};
use crate::plugin_permissions::{Permission, PluginPermissions};
use crate::runtime::{
    HintContent, LiveCtx, PromptHintCallbacks, PromptHintRegistration, RecencySourceCallbacks,
    RecencySourceRegistration, Request, command_depth,
};

const TOOL_NAME_MAX: usize = 64;
const TOOL_HANDLER_RETURN_ERR: &str =
    "tool handler must return string or {output=string, is_error?=bool}";
const TIMEOUT_PARSE_ERR: &str = "register_tool: 'timeout' must be a positive number, 0, or false";
const NARGS_ERR: &str = r#"register_command: 'nargs' must be 0, 1, "?", "*", or "+""#;
const PERMISSION_RULE_KEYS: &[&str] = &["tool", "scope", "effect"];
const MAX_HINT_CONTENT_SIZE: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) enum PermissionScopeKind {
    Field(Arc<str>),
    Callback,
}

pub(crate) enum PermissionScopeSpec {
    Field(Arc<str>),
    Callback(RegistryKey),
}

impl PermissionScopeSpec {
    fn kind(&self) -> PermissionScopeKind {
        match self {
            Self::Field(f) => PermissionScopeKind::Field(Arc::clone(f)),
            Self::Callback(_) => PermissionScopeKind::Callback,
        }
    }

    pub(crate) fn callback_key(self) -> Option<RegistryKey> {
        match self {
            Self::Callback(key) => Some(key),
            Self::Field(_) => None,
        }
    }
}

/// What a tool exposes to the model: the capability, plus the scopes every
/// call is checked against. One field rather than two, because a capability
/// without scopes is never checked and scopes without a capability cannot be
/// delegated, so neither half can exist alone.
pub(crate) struct ToolPermission<S> {
    pub(crate) permission: Permission,
    pub(crate) scopes: S,
}

impl ToolPermission<PermissionScopeSpec> {
    pub(crate) fn kind(&self) -> ToolPermission<PermissionScopeKind> {
        ToolPermission {
            permission: self.permission,
            scopes: self.scopes.kind(),
        }
    }
}

pub(crate) struct PendingTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) handler_key: RegistryKey,
    pub(crate) header_key: Option<RegistryKey>,
    pub(crate) restore_key: Option<RegistryKey>,
    pub(crate) permission: Option<ToolPermission<PermissionScopeSpec>>,
    pub(crate) mutable_path_field: Option<Arc<str>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) kind: Option<Arc<str>>,
}

impl PendingTool {
    /// Every lua registry key the tool holds, so a rollback frees one list
    /// instead of one key per call site.
    pub(crate) fn registry_keys(self) -> impl Iterator<Item = RegistryKey> {
        [
            Some(self.handler_key),
            self.header_key,
            self.restore_key,
            self.permission.and_then(|p| p.scopes.callback_key()),
        ]
        .into_iter()
        .flatten()
    }
}

pub(crate) type PendingTools = Arc<Mutex<Vec<PendingTool>>>;

/// A rule as declared, before it is checked against the tool it names. Native
/// and scoped by construction. [`resolve_rules`] turns it into the effective
/// [`PermissionRule`].
pub(crate) struct PendingRule {
    tool: Arc<str>,
    scope: String,
    effect: Effect,
}

pub(crate) type PendingRules = Arc<Mutex<Vec<PendingRule>>>;

pub(crate) struct LuaTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) tx: Sender<Request>,
    pub(crate) plugin: Arc<str>,
    pub(crate) has_header_fn: bool,
    pub(crate) permission: Option<ToolPermission<PermissionScopeKind>>,
    pub(crate) mutable_path_field: Option<Arc<str>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) kind: Option<Arc<str>>,
}

impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(&self.description)
    }

    fn schema(&self) -> Value {
        to_json_schema(self.schema)
    }

    fn audience(&self) -> ToolAudience {
        self.audience
    }

    fn required_permission(&self) -> Option<Permission> {
        Some(self.permission.as_ref()?.permission)
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let validated = validate(self.schema, input.clone())?;
        let permission_state = match self.permission.as_ref().map(|p| &p.scopes) {
            Some(PermissionScopeKind::Field(field)) => {
                let scope = validated.get(field.as_ref()).and_then(|v| v.as_str());
                PermissionState::Ready(Some(match scope {
                    Some(s) => PermissionScopes::single(s.to_owned()),
                    None => PermissionScopes::force_prompt(validated.to_string()),
                }))
            }
            Some(PermissionScopeKind::Callback) => PermissionState::NeedsCompute,
            None => PermissionState::Ready(None),
        };
        Ok(Box::new(LuaToolInvocation {
            tool: Arc::clone(&self.name),
            plugin: Arc::clone(&self.plugin),
            has_header_fn: self.has_header_fn,
            input: validated,
            tx: self.tx.clone(),
            permission_state,
            mutable_path_field: self.mutable_path_field.clone(),
            timeout: self.timeout,
        }))
    }

    fn tool_kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }
}

enum PermissionState {
    Ready(Option<PermissionScopes>),
    NeedsCompute,
}

struct LuaToolInvocation {
    tool: Arc<str>,
    plugin: Arc<str>,
    has_header_fn: bool,
    input: Value,
    tx: Sender<Request>,
    permission_state: PermissionState,
    mutable_path_field: Option<Arc<str>>,
    timeout: Option<Duration>,
}

impl ToolInvocation for LuaToolInvocation {
    fn start_header(&self) -> HeaderFuture {
        if !self.has_header_fn {
            return HeaderFuture::Ready(HeaderResult::plain(self.tool.to_string()));
        }
        let (reply_tx, reply_rx) = flume::bounded::<HeaderResult>(1);
        let tool = Arc::clone(&self.tool);
        let plugin = Arc::clone(&self.plugin);
        let input = self.input.clone();
        let tx = self.tx.clone();
        let fallback = tool.to_string();
        HeaderFuture::Pending {
            fallback: fallback.clone(),
            fut: Box::pin(async move {
                let sent = tx
                    .send_async(Request::ComputeHeader {
                        plugin: Arc::clone(&plugin),
                        tool: Arc::clone(&tool),
                        input,
                        reply: reply_tx,
                    })
                    .await;
                if sent.is_err() {
                    return HeaderResult::plain(fallback);
                }
                reply_rx
                    .recv_async()
                    .await
                    .unwrap_or_else(|_| HeaderResult::plain(fallback))
            }),
        }
    }

    fn mutable_path(&self) -> Option<&Path> {
        let field = self.mutable_path_field.as_deref()?;
        let val = self.input.get(field)?.as_str()?;
        Some(Path::new(val))
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        match &self.permission_state {
            PermissionState::Ready(v) => Box::pin(std::future::ready(v.clone())),
            PermissionState::NeedsCompute => {
                let (reply_tx, reply_rx) = flume::bounded(1);
                let tx = self.tx.clone();
                let plugin = Arc::clone(&self.plugin);
                let tool = Arc::clone(&self.tool);
                let input = self.input.clone();
                let fallback = input.to_string();
                Box::pin(async move {
                    if tx
                        .send_async(Request::ComputePermissionScopes {
                            plugin,
                            tool,
                            input,
                            reply: reply_tx,
                        })
                        .await
                        .is_err()
                    {
                        return Some(PermissionScopes::force_prompt(fallback));
                    }
                    match reply_rx.recv_async().await {
                        Ok(Some(scopes)) => Some(scopes),
                        _ => Some(PermissionScopes::force_prompt(fallback)),
                    }
                })
            }
        }
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        let deadline = ctx.deadline;
        let plugin = self.plugin;
        let tool = self.tool;
        let input = self.input;
        let tx = self.tx;
        let tool_timeout = self.timeout;

        Box::pin(async move {
            let effective_secs: Option<u64> = match tool_timeout {
                Some(d) => match deadline.cap_timeout(d.as_secs()) {
                    Ok(s) => Some(s),
                    Err(e) => return Err(e).into(),
                },
                None => match deadline {
                    Deadline::At(_) => match deadline.cap_timeout(u64::MAX) {
                        Ok(s) => Some(s),
                        Err(e) => return Err(e).into(),
                    },
                    Deadline::None => None,
                },
            };

            let (reply_tx, reply_rx) = flume::bounded::<ToolCallReply>(1);
            let live = ctx.tool_use_id.clone().map(|id| LiveCtx {
                event_tx: ctx.event_tx.clone(),
                tool_use_id: id,
            });
            let lua_ctx = LuaCtx {
                cancel: ctx.cancel.clone(),
                config: ctx.config.clone(),
                tool_output_lines: ctx.tool_output_lines,
                finish_tx: None,
                file_tracker: ctx.file_tracker.clone(),
                loaded_instructions: ctx.loaded_instructions.clone(),
                session_id: ctx.session_id.clone(),
            };

            if tx
                .send_async(Request::CallTool {
                    plugin: Arc::clone(&plugin),
                    tool: Arc::clone(&tool),
                    input,
                    ctx: Box::new(lua_ctx),
                    deadline: match deadline {
                        Deadline::At(t) => Some(t),
                        Deadline::None => None,
                    },
                    reply: reply_tx,
                    live,
                })
                .await
                .is_err()
            {
                return Err("lua thread disconnected".to_string()).into();
            }

            let recv = async { Some(reply_rx.recv_async().await) };
            let result = match effective_secs {
                Some(secs) => {
                    tokio::select! {
                        result = recv => result,
                        _ = tokio::time::sleep(Duration::from_secs(secs)) => None,
                    }
                }
                None => recv.await,
            };

            match result {
                None => Err(format!(
                    "plugin {} tool {} exceeded timeout ({}s)",
                    plugin,
                    tool,
                    effective_secs.unwrap_or(0)
                ))
                .into(),
                Some(Err(_)) => Err("lua thread disconnected".to_string()).into(),
                Some(Ok(reply)) => {
                    if let Some(ref id) = ctx.tool_use_id {
                        if let Some(live_buf) = reply.live_buf {
                            crate::runtime::send_render_event(
                                &ctx.event_tx,
                                id,
                                "live_buf",
                                AgentEvent::LiveToolBuf {
                                    id: id.clone(),
                                    body: live_buf,
                                },
                            );
                        }
                        crate::runtime::RestoreReply {
                            body: reply.snapshot,
                            header: reply.header,
                        }
                        .emit(id, None, &ctx.event_tx);
                    }
                    let format = reply.format;
                    let image = reply.image;
                    ToolExecResult {
                        output: reply.result.map(|s| {
                            if let Some(source) = image {
                                ToolOutput::Image { caption: s, source }
                            } else {
                                match format {
                                    LuaOutputFormat::Markdown => ToolOutput::Markdown(s),
                                    LuaOutputFormat::Plain => ToolOutput::Plain(s),
                                }
                            }
                        }),
                        annotation: reply.annotation,
                        written_path: reply.written_path,
                    }
                }
            }
        })
    }
}

fn parse_slot(spec: &Table) -> LuaResult<Slot> {
    spec.get::<String>("slot")
        .map_err(|_| mlua::Error::runtime("'slot' is required"))?
        .parse()
        .map_err(|_| {
            mlua::Error::runtime(format!("unknown 'slot'. Valid: {}", Slot::valid_names()))
        })
}

fn parse_prompt_field(spec: &Table) -> LuaResult<Option<Vec<PromptId>>> {
    let parse_one = |s: &str| -> LuaResult<PromptId> {
        s.parse().map_err(|_| {
            mlua::Error::runtime(format!(
                "unknown 'prompt'. Valid: {}",
                PromptId::valid_names()
            ))
        })
    };
    match spec.get::<LuaValue>("prompt") {
        Ok(LuaValue::String(s)) => Ok(Some(vec![parse_one(&s.to_str()?)?])),
        Ok(LuaValue::Table(t)) => {
            let mut ids = Vec::new();
            for pair in t.sequence_values::<mlua::LuaString>() {
                ids.push(parse_one(&pair?.to_str()?)?);
            }
            if ids.is_empty() {
                return Err(mlua::Error::runtime(
                    "'prompt' table is empty or has no sequence entries; expected a list like {\"system\", \"general\"}",
                ));
            }
            Ok(Some(ids))
        }
        Ok(LuaValue::Nil) | Err(_) => Ok(None),
        Ok(_) => Err(mlua::Error::runtime(
            "'prompt' must be a string or list of strings",
        )),
    }
}

fn validate_slot_prompt_compatibility(
    slot: Slot,
    prompts: &Option<Vec<PromptId>>,
) -> LuaResult<()> {
    if let Some(prompts) = prompts {
        for &pid in prompts {
            if !pid.has_slot(slot) {
                return Err(mlua::Error::runtime(format!(
                    "slot '{}' is not available for prompt '{}'",
                    slot, pid
                )));
            }
        }
    }
    Ok(())
}

fn parse_hint_content(lua: &Lua, spec: &Table) -> LuaResult<HintContent> {
    if !spec.contains_key("content")? {
        return Err(mlua::Error::runtime("'content' is required"));
    }
    match spec.get("content")? {
        LuaValue::String(s) => {
            let text = s.to_string_lossy();
            if text.is_empty() {
                return Err(mlua::Error::runtime("'content' must not be empty"));
            }
            if text.len() > MAX_HINT_CONTENT_SIZE {
                return Err(mlua::Error::runtime(format!(
                    "content exceeds the {} byte limit",
                    MAX_HINT_CONTENT_SIZE
                )));
            }
            Ok(HintContent::Static(text))
        }
        LuaValue::Function(f) => Ok(HintContent::Callback(lua.create_registry_value(f)?)),
        _ => Err(mlua::Error::runtime(
            "'content' must be a string or function",
        )),
    }
}

pub(crate) fn create_api_table(
    lua: &Lua,
    pending: PendingTools,
    pending_rules: PendingRules,
    permissions: PluginPermissions,
    plugin: Arc<str>,
    opts: crate::api::options::PluginOpts,
    ui_action_tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "register_options",
        crate::api::options::create_register_options_fn(lua, Arc::clone(&plugin), opts)?,
    )?;

    {
        let pending = Arc::clone(&pending);
        let permissions = permissions.clone();
        t.set(
            "register_tool",
            lua.create_function(move |lua, spec: Table| {
                register_tool_from_lua(lua, &spec, pending.clone(), &permissions)
            })?,
        )?;
    }

    t.set(
        "register_permission_rule",
        lua.create_function(move |_lua, spec: Table| {
            register_permission_rule(&spec, &pending_rules)
        })?,
    )?;

    {
        let registry = lua
            .app_data_ref::<Arc<ToolRegistry>>()
            .map(|r| Arc::clone(&r));
        t.set(
            "has_tool",
            lua.create_function(move |_lua, name: String| match &registry {
                Some(registry) => Ok(registry.has(&name)),
                None => Err(mlua::Error::runtime(
                    "has_tool: tool registry not available",
                )),
            })?,
        )?;
    }

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "register_prompt_hint",
            lua.create_function(move |lua, spec: Table| {
                let slot = parse_slot(&spec)?;
                if slot.kind() == SlotKind::Singleton {
                    return Err(mlua::Error::runtime(format!(
                        "register_prompt_hint is for aggregate slots ({}); \
                         use set_prompt for singleton slots ({})",
                        Slot::names_for_kind(SlotKind::Aggregate),
                        Slot::names_for_kind(SlotKind::Singleton),
                    )));
                }
                let prompts = parse_prompt_field(&spec)?;
                validate_slot_prompt_compatibility(slot, &prompts)?;

                let content = parse_hint_content(lua, &spec)?;
                let reg = PromptHintRegistration {
                    prompts,
                    slot,
                    content,
                };
                let mut map = lua
                    .app_data_mut::<PromptHintCallbacks>()
                    .ok_or_else(|| mlua::Error::runtime("not initialized"))?;
                map.entry(Arc::clone(&plugin)).or_default().push(reg);
                Ok(())
            })?,
        )?;
    }

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "set_prompt",
            lua.create_function(move |lua, spec: Table| {
                let slot = parse_slot(&spec)?;
                if slot.kind() == SlotKind::Aggregate {
                    return Err(mlua::Error::runtime(format!(
                        "set_prompt is for singleton slots ({}); \
                         use register_prompt_hint for aggregate slots ({})",
                        Slot::names_for_kind(SlotKind::Singleton),
                        Slot::names_for_kind(SlotKind::Aggregate),
                    )));
                }

                let prompts = parse_prompt_field(&spec)?;
                validate_slot_prompt_compatibility(slot, &prompts)?;

                let content = parse_hint_content(lua, &spec)?;
                let reg = PromptHintRegistration {
                    prompts,
                    slot,
                    content,
                };
                let mut map = lua
                    .app_data_mut::<PromptHintCallbacks>()
                    .ok_or_else(|| mlua::Error::runtime("not initialized"))?;
                map.entry(Arc::clone(&plugin)).or_default().push(reg);
                Ok(())
            })?,
        )?;
    }

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "register_recency_source",
            lua.create_function(move |lua, spec: Table| {
                let name: String = spec.get("name").map_err(|_| {
                    mlua::Error::runtime("register_recency_source: 'name' is required")
                })?;
                if name.is_empty() {
                    return Err(mlua::Error::runtime(
                        "register_recency_source: 'name' must not be empty",
                    ));
                }
                let func: Function = spec.get("callback").map_err(|_| {
                    mlua::Error::runtime("register_recency_source: 'callback' is required")
                })?;
                let callback = lua.create_registry_value(func)?;
                let reg = RecencySourceRegistration {
                    name: Arc::from(name),
                    callback,
                };
                let mut map = lua
                    .app_data_mut::<RecencySourceCallbacks>()
                    .ok_or_else(|| mlua::Error::runtime("not initialized"))?;
                map.entry(Arc::clone(&plugin)).or_default().push(reg);
                Ok(())
            })?,
        )?;
    }

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "register_hook",
            lua.create_function(move |lua, spec: Table| {
                register_hook_from_lua(lua, &spec, Arc::clone(&plugin))
            })?,
        )?;
    }

    t.set(
        "register_command",
        lua.create_function(move |lua, spec: Table| {
            register_command_from_lua(lua, &spec, Arc::clone(&plugin))
        })?,
    )?;

    if let Some(tx) = ui_action_tx {
        let run_tx = tx.clone();
        t.set(
            "run_command",
            lua.create_async_function(move |lua: Lua, cmdline: String| {
                let tx = run_tx.clone();
                async move { run_command_pair(lua, tx, cmdline).await }
            })?,
        )?;
    }

    Ok(t)
}

/// Runs a slash command by name, exactly as typing it in the input would.
/// Works for built-ins, custom `/project:` and `/user:` commands, MCP
/// prompts, and commands other plugins registered.
///
/// Use it to alias a command you like under a name you prefer, instead of
/// reimplementing what it does. See `craft.ui.action` for the same idea
/// applied to keybound UI actions.
///
/// Pass the whole command line, arguments included: `"/cd ~/src"`. The
/// leading slash is optional. Names match exactly apart from case, so a typo
/// reports an error instead of running the closest command, and a cycle of
/// aliases stops with one too.
///
/// This returns as soon as the command has been dispatched, not when it
/// finishes, so aliasing something long-running like `/compact` does not
/// block your handler.
async fn run_command_pair(
    lua: Lua,
    tx: flume::Sender<UiAction>,
    cmdline: String,
) -> LuaResult<Pair<bool>> {
    let depth = command_depth(&lua).saturating_add(1);
    let reply = try_pair!(
        ui_roundtrip(Some(&tx), |reply_tx| UiAction::RunCommand {
            cmdline,
            depth,
            reply_tx,
        })
        .await
    );
    try_pair!(reply);
    Ok((Some(true), None))
}

fn is_valid_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > TOOL_NAME_MAX {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_audience(audiences: Option<mlua::Table>) -> LuaResult<ToolAudience> {
    let Some(arr) = audiences else {
        return Ok(ToolAudience::default());
    };
    let mut flags = ToolAudience::empty();
    let mut count = 0;
    for item in arr.sequence_values::<String>() {
        let s = item?;
        count += 1;
        flags |= match s.as_str() {
            "all" => ToolAudience::all(),
            "main" => ToolAudience::MAIN,
            "research_sub" => ToolAudience::RESEARCH_SUB,
            "general_sub" => ToolAudience::GENERAL_SUB,
            "interpreter" => ToolAudience::INTERPRETER,
            _ => {
                return Err(mlua::Error::runtime(format!("unknown audience: {s}")));
            }
        };
    }
    if count == 0 {
        return Err(mlua::Error::runtime(
            "register_tool: 'audiences' must be omitted or non-empty",
        ));
    }
    Ok(flags)
}

fn parse_timeout(spec: &Table) -> LuaResult<Option<Duration>> {
    let value: LuaValue = spec.get("timeout").unwrap_or(LuaValue::Nil);
    match value {
        LuaValue::Nil | LuaValue::Boolean(false) => Ok(None),
        LuaValue::Integer(0) => Ok(None),
        LuaValue::Integer(n) if n > 0 => Ok(Some(Duration::from_secs(n as u64))),
        LuaValue::Number(n) if n > 0.0 && n.is_finite() => Ok(Some(Duration::from_secs(n as u64))),
        LuaValue::Number(0.0) => Ok(None),
        _ => Err(mlua::Error::runtime(TIMEOUT_PARSE_ERR)),
    }
}

fn require_string_field(spec: &Table, key: &str, schema: &Value) -> LuaResult<Option<Arc<str>>> {
    let field: Option<Arc<str>> = spec.get::<String>(key).ok().map(|s| Arc::from(s.as_str()));
    if let Some(ref field) = field {
        check_schema_field(schema, key, field)?;
    }
    Ok(field)
}

fn check_schema_field(schema: &Value, key: &str, field: &str) -> LuaResult<()> {
    let is_string = schema
        .get("properties")
        .and_then(|p| p.get(field))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == "string");
    if !is_string {
        return Err(mlua::Error::runtime(format!(
            "register_tool: {key} field '{field}' not in schema properties or not type 'string'"
        )));
    }
    Ok(())
}

/// Declare an agent permission rule for a native tool. Use it to pre-allow
/// (or pre-deny) tool calls on paths your plugin owns, like a storage
/// directory outside the working dir, so the user is not prompted for them.
///
/// An allow is delegation, not escalation: it needs the `permission` the
/// target tool declares, so a plugin can only pre-approve what it could
/// already do itself. A deny needs no permission.
///
/// Allows are checked once the plugin finishes loading, so a plugin may
/// pre-approve a tool it registers itself. One that does not hold up (no such
/// tool, a tool with no `permission_scopes` that is never checked, or a
/// permission the plugin lacks) is dropped with a warning in the log while the
/// rest of the plugin loads. Without the rule the call simply prompts.
///
/// Rules live as long as the plugin is loaded: a reload replaces them, and a
/// reload that registers none clears the old ones. User config and session
/// deny rules always win over a plugin allow.
///
/// `spec.tool` is a required native tool name (no wildcard or MCP),
/// `spec.scope` a required non-empty pattern, `spec.effect` an optional
/// "allow" (default) or "deny".
fn register_permission_rule(spec: &Table, pending_rules: &PendingRules) -> LuaResult<()> {
    for entry in spec.pairs::<String, LuaValue>() {
        let (key, _) = entry.map_err(|_| {
            mlua::Error::runtime("register_permission_rule: spec keys must be strings")
        })?;
        if !PERMISSION_RULE_KEYS.contains(&key.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "register_permission_rule: unknown key '{key}' (valid: tool, scope, effect)"
            )));
        }
    }

    let tool: String = spec.get("tool").map_err(|_| {
        mlua::Error::runtime("register_permission_rule: 'tool' must be a native tool name string")
    })?;
    let tool = match ToolKey::parse(&tool) {
        Ok(ToolKey::Native(name)) => name,
        Ok(_) => {
            return Err(mlua::Error::runtime(
                "register_permission_rule: only native tools are allowed (no wildcard or MCP)",
            ));
        }
        Err(e) => {
            return Err(mlua::Error::runtime(format!(
                "register_permission_rule: {e}"
            )));
        }
    };

    let scope: String = spec
        .get("scope")
        .map_err(|_| mlua::Error::runtime("register_permission_rule: 'scope' must be a string"))?;
    if scope.is_empty() {
        return Err(mlua::Error::runtime(
            "register_permission_rule: 'scope' must be non-empty",
        ));
    }

    let effect = spec
        .get::<Option<String>>("effect")
        .map_err(|_| mlua::Error::runtime("register_permission_rule: 'effect' must be a string"))?;
    let effect = match effect.as_deref() {
        None | Some("allow") => Effect::Allow,
        Some("deny") => Effect::Deny,
        Some(other) => {
            return Err(mlua::Error::runtime(format!(
                "register_permission_rule: invalid effect '{other}' (expected \"allow\" or \"deny\")"
            )));
        }
    };

    // A deny that covers everything is the safest rule a plugin can write, so
    // only an allow is refused.
    if effect == Effect::Allow && is_universal_scope(&scope) {
        return Err(mlua::Error::runtime(format!(
            "register_permission_rule: '{scope}' matches every scope; name the paths or commands the rule covers"
        )));
    }

    pending_rules
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(PendingRule {
            tool,
            scope,
            effect,
        });
    Ok(())
}

/// Turns the rules a load declared into the rules that take effect. Runs once,
/// at the commit point of that load, so the answer never depends on which
/// plugin happened to run first and the plugin's own tools already exist.
///
/// A rule that does not survive is dropped with a warning rather than failing
/// the load: losing an allow only puts the call back in front of the user,
/// which is no reason to take the plugin's tools and commands down with it.
pub(crate) fn resolve_rules(
    registry: &ToolRegistry,
    plugin: &str,
    permissions: &PluginPermissions,
    rules: Vec<PendingRule>,
) -> Vec<PermissionRule> {
    rules
        .into_iter()
        .filter(|rule| {
            rule.effect != Effect::Allow || allow_is_delegated(registry, plugin, permissions, rule)
        })
        .map(|rule| PermissionRule {
            tool: ToolKey::Native(rule.tool),
            scope: Some(rule.scope),
            effect: rule.effect,
        })
        .collect()
}

/// An allow has to name a tool that exists, is permission checked at all, and
/// exposes no more than the plugin already holds.
fn allow_is_delegated(
    registry: &ToolRegistry,
    plugin: &str,
    permissions: &PluginPermissions,
    rule: &PendingRule,
) -> bool {
    let dropped = |reason: &str| {
        tracing::warn!(plugin, tool = %rule.tool, reason, "permission rule dropped");
        false
    };
    let Some(registered) = registry.get(&rule.tool) else {
        // Disabled by config, or owned by a plugin nobody loaded.
        return dropped("no such tool is registered");
    };
    let Some(required) = registered.tool.required_permission() else {
        return dropped(
            "the tool declares no permission_scopes, so it is never permission checked",
        );
    };
    permissions.is_allowed(required)
        || dropped(&format!(
            "allowing it exposes '{required}', which this plugin was not granted"
        ))
}

/// Reads both keys together, so the pair enters the runtime whole and only
/// when the plugin holds what the tool would expose.
fn parse_tool_permission(
    lua: &Lua,
    spec: &Table,
    tool: &str,
    schema: &Value,
    permissions: &PluginPermissions,
) -> LuaResult<Option<ToolPermission<PermissionScopeSpec>>> {
    let declared = spec
        .get::<Option<String>>("permission")?
        .map(|key| {
            Permission::from_key(&key).ok_or_else(|| {
                mlua::Error::runtime(format!(
                    "register_tool: unknown permission '{key}' (valid: {})",
                    Permission::ALL
                        .iter()
                        .map(|p| p.manifest_key())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .transpose()?;
    let scopes = spec.get::<LuaValue>("permission_scopes")?;

    let permission = match (declared, scopes.is_nil()) {
        (None, true) => return Ok(None),
        (Some(permission), false) => permission,
        (None, false) => {
            return Err(mlua::Error::runtime(
                "register_tool: a tool with 'permission_scopes' must declare 'permission', the capability it exposes to the model",
            ));
        }
        (Some(_), true) => {
            return Err(mlua::Error::runtime(
                "register_tool: 'permission' needs 'permission_scopes', without scopes the tool is never permission checked",
            ));
        }
    };
    if !permissions.is_allowed(permission) {
        return Err(mlua::Error::runtime(format!(
            "register_tool: '{tool}' exposes '{permission}' to the model, which this plugin was not granted"
        )));
    }

    let scopes = match scopes {
        LuaValue::String(s) => {
            let field = s.to_str()?.to_owned();
            check_schema_field(schema, "permission_scopes", &field)?;
            PermissionScopeSpec::Field(Arc::from(field.as_str()))
        }
        LuaValue::Function(f) => PermissionScopeSpec::Callback(lua.create_registry_value(f)?),
        _ => {
            return Err(mlua::Error::runtime(
                "register_tool: 'permission_scopes' must be a string field name or a function",
            ));
        }
    };
    Ok(Some(ToolPermission { permission, scopes }))
}

fn register_tool_from_lua(
    lua: &Lua,
    spec: &Table,
    pending: PendingTools,
    permissions: &PluginPermissions,
) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'name'"))?;
    if !is_valid_tool_name(&name) {
        return Err(mlua::Error::runtime(format!(
            "register_tool: invalid name '{name}'"
        )));
    }
    let description: String = spec.get("description").unwrap_or_default();
    if description.trim().is_empty() {
        return Err(mlua::Error::runtime(
            "register_tool: description must be non-empty",
        ));
    }
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'handler'"))?;
    let schema_table: LuaValue = spec
        .get("schema")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'schema'"))?;
    let audiences: Option<mlua::Table> = spec.get("audiences").ok();

    let schema_val: Value = lua.from_value(schema_table)?;
    let param_schema = try_from_json(&schema_val).map_err(mlua::Error::runtime)?;

    if !spec.get::<LuaValue>("permission_scope")?.is_nil() {
        return Err(mlua::Error::runtime(
            "register_tool: 'permission_scope' was removed; use permission_scopes = \"<field>\" or permission_scopes = function(input) ... end",
        ));
    }
    let mutable_path_field = require_string_field(spec, "mutable_path", &schema_val)?;

    let permission = parse_tool_permission(lua, spec, &name, &schema_val, permissions)?;

    let header_fn: Option<Function> = spec.get("header").ok();
    let restore_fn: Option<Function> = spec.get("restore").ok();
    let audience = parse_audience(audiences)?;
    let timeout = parse_timeout(spec)?;
    let kind: Option<Arc<str>> = spec
        .get::<String>("kind")
        .ok()
        .map(|s| Arc::from(s.as_str()));
    let handler_key: RegistryKey = lua.create_registry_value(handler)?;
    let header_key = header_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let restore_key = restore_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let name: Arc<str> = Arc::from(name.as_str());

    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(PendingTool {
            name,
            description,
            schema: param_schema,
            audience,
            handler_key,
            header_key,
            restore_key,
            permission,
            mutable_path_field,
            timeout,
            kind,
        });

    Ok(())
}

/// Matching only needs an upper bound, so "+" and "*" both become MAX;
/// minimums ("+" vs "*") are left for handlers to enforce.
fn parse_nargs(spec: &Table) -> LuaResult<usize> {
    match spec.get::<LuaValue>("nargs")? {
        LuaValue::Nil | LuaValue::Integer(0) | LuaValue::Number(0.0) => Ok(0),
        LuaValue::Integer(1) | LuaValue::Number(1.0) => Ok(1),
        LuaValue::String(s) => match s.to_string_lossy().as_ref() {
            "?" => Ok(1),
            "*" | "+" => Ok(usize::MAX),
            _ => Err(mlua::Error::runtime(NARGS_ERR)),
        },
        _ => Err(mlua::Error::runtime(NARGS_ERR)),
    }
}

fn register_command_from_lua(lua: &Lua, spec: &Table, plugin: Arc<str>) -> LuaResult<()> {
    let mut name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'name'"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_command: name must be non-empty",
        ));
    }
    if !name.starts_with('/') {
        name.insert(0, '/');
    }
    let description: String = spec.get("description").unwrap_or_default();
    let max_args = parse_nargs(spec)?;
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'handler'"))?;

    let handler_key = lua.create_registry_value(handler)?;
    let name: Arc<str> = Arc::from(name.as_str());
    let description: Arc<str> = Arc::from(description.as_str());

    {
        let mut map = lua
            .app_data_mut::<CommandHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
        map.entry(Arc::clone(&plugin)).or_default().insert(
            Arc::clone(&name),
            CommandEntry {
                handler: handler_key,
                description,
                max_args,
            },
        );
    }

    let map = lua
        .app_data_ref::<CommandHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    let writer = lua
        .app_data_ref::<LuaCommandWriter>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    publish_command_snapshot(&map, &writer);

    Ok(())
}

fn register_hook_from_lua(lua: &Lua, spec: &Table, plugin: Arc<str>) -> LuaResult<()> {
    let event: String = spec
        .get("event")
        .map_err(|_| mlua::Error::runtime("register_hook: missing 'event'"))?;
    let valid = matches!(
        event.as_str(),
        super::hooks::EVENT_SESSION_START
            | super::hooks::EVENT_PRE_TOOL_USE
            | super::hooks::EVENT_POST_TOOL_USE
    );
    if !valid {
        return Err(mlua::Error::runtime(format!(
            "register_hook: invalid event '{event}'. Must be session_start, pre_tool_use, or post_tool_use."
        )));
    }
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_hook: missing 'handler'"))?;
    let handler_key = lua.create_registry_value(handler)?;

    let mut map = lua
        .app_data_mut::<super::hooks::HookHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("register_hook: not initialized"))?;
    map.entry(event).or_default().push((plugin, handler_key));

    Ok(())
}

pub(crate) type ToolCallResult = Result<String, String>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LuaOutputFormat {
    #[default]
    Plain,
    Markdown,
}

const LUA_FORMAT_MARKDOWN: &str = "markdown";
const LUA_FORMAT_PLAIN: &str = "plain";

pub(crate) struct ToolCallReply {
    pub result: ToolCallResult,
    pub snapshot: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
    pub live_buf: Option<Arc<SharedBuf>>,
    pub format: LuaOutputFormat,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
    /// Set via `image = { media_type = "image/png", data = <base64> }` in the
    /// handler return; becomes `ToolOutput::Image` with `result` as caption.
    pub image: Option<ImageSource>,
}

impl ToolCallReply {
    pub fn from_lua_value(val: &LuaValue) -> Self {
        let mut result = coerce_tool_result(val);
        let LuaValue::Table(t) = val else {
            return Self {
                result,
                snapshot: None,
                header: None,
                live_buf: None,
                format: LuaOutputFormat::default(),
                annotation: None,
                written_path: None,
                image: None,
            };
        };
        let (snapshot, live_buf) = Self::extract_body_handle(t);
        let header = t
            .get::<LuaValue>("header")
            .ok()
            .and_then(|v| Self::extract_snapshot(&v));
        let format = extract_format(t);
        let annotation = t.get::<String>("annotation").ok();
        let written_path = t.get::<String>("written_path").ok();
        // A malformed image fails the call; dropping it silently would leave
        // a caption claiming pixels the model never receives.
        let image = match extract_image(t) {
            Ok(image) => image,
            Err(e) => {
                result = Err(e);
                None
            }
        };
        Self {
            result,
            snapshot,
            header,
            live_buf,
            format,
            annotation,
            written_path,
            image,
        }
    }

    fn extract_body_handle(t: &mlua::Table) -> (Option<BufferSnapshot>, Option<Arc<SharedBuf>>) {
        t.get::<LuaValue>("body")
            .ok()
            .and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some((Some(h.buf.take()), Some(Arc::clone(&h.buf))))
            })
            .unwrap_or((None, None))
    }

    fn extract_snapshot(val: &LuaValue) -> Option<BufferSnapshot> {
        let ud = val.as_userdata()?;
        let h = ud.borrow::<BufHandle>().ok()?;
        Some(h.buf.take())
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            result: Err(msg.into()),
            snapshot: None,
            header: None,
            live_buf: None,
            format: LuaOutputFormat::default(),
            annotation: None,
            written_path: None,
            image: None,
        }
    }
}

fn extract_image(t: &mlua::Table) -> Result<Option<ImageSource>, String> {
    let entry = match t.get::<LuaValue>("image") {
        Ok(LuaValue::Table(entry)) => entry,
        Ok(LuaValue::Nil) | Err(_) => return Ok(None),
        Ok(other) => {
            return Err(format!(
                "tool 'image' field must be a table {{ media_type, data }}, got {}",
                other.type_name()
            ));
        }
    };
    let media_type = entry
        .get::<String>("media_type")
        .map_err(|_| "tool image is missing 'media_type'".to_owned())?;
    let media_type = ImageMediaType::from_mime(&media_type).ok_or_else(|| {
        let supported: Vec<&str> = ImageMediaType::ALL.iter().map(|m| m.as_mime()).collect();
        format!(
            "unsupported tool image media_type '{media_type}' ({})",
            supported.join(", ")
        )
    })?;
    let data = entry
        .get::<String>("data")
        .map_err(|_| "tool image is missing base64 'data'".to_owned())?;
    // Bad base64 would land in history and fail every later request;
    // validate once at the boundary.
    if data.is_empty() {
        return Err("tool image 'data' is empty".to_owned());
    }
    BASE64
        .decode(data.as_bytes())
        .map_err(|e| format!("tool image 'data' is not valid base64: {e}"))?;
    Ok(Some(ImageSource::new(media_type, Arc::from(data))))
}

fn extract_format(t: &mlua::Table) -> LuaOutputFormat {
    let Ok(LuaValue::String(s)) = t.get::<LuaValue>("format") else {
        return LuaOutputFormat::default();
    };
    let Ok(s) = s.to_str() else {
        return LuaOutputFormat::default();
    };
    match &*s {
        LUA_FORMAT_MARKDOWN => LuaOutputFormat::Markdown,
        LUA_FORMAT_PLAIN => LuaOutputFormat::Plain,
        _ => LuaOutputFormat::default(),
    }
}

pub(crate) fn coerce_tool_result(result: &LuaValue) -> ToolCallResult {
    match result {
        LuaValue::String(s) => s.to_str().map(|s| s.to_owned()).map_err(|e| e.to_string()),
        LuaValue::Table(t) => {
            let output = t.get::<LuaValue>("llm_output").ok().and_then(|v| {
                if let LuaValue::String(s) = v {
                    s.to_str().ok().map(|s| s.to_owned())
                } else {
                    None
                }
            });
            match output {
                Some(s) if matches!(t.get::<LuaValue>("is_error"), Ok(LuaValue::Boolean(true))) => {
                    Err(s)
                }
                Some(s) => Ok(s),
                None => Err(TOOL_HANDLER_RETURN_ERR.to_string()),
            }
        }
        _ => Err(TOOL_HANDLER_RETURN_ERR.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case::test_case("echo", true ; "simple_name")]
    #[test_case::test_case("tool123", true ; "trailing_digits")]
    #[test_case::test_case("", false ; "empty")]
    #[test_case::test_case("../../bash", false ; "path_traversal")]
    #[test_case::test_case("foo bar", false ; "space")]
    #[test_case::test_case("1foo", false ; "leading_digit")]
    fn tool_name_validation(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
    }

    #[test_case::test_case(
        r#"{ llm_output = "c", image = { data = "aGVsbG8=" } }"#,
        "missing 'media_type'" ; "missing_media_type")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png" } }"#,
        "missing base64 'data'" ; "missing_data")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png", data = "" } }"#,
        "'data' is empty" ; "empty_data")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = "nope" }"#,
        "must be a table" ; "image_not_a_table")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/bmp", data = "aGVsbG8=" } }"#,
        "unsupported tool image media_type" ; "unsupported_media_type")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png", data = "!!!not base64!!!" } }"#,
        "not valid base64" ; "data_not_base64")]
    fn malformed_image_reply_fails_the_call(src: &str, expected: &str) {
        let lua = Lua::new();
        let val: LuaValue = lua.load(format!("return {src}")).eval().unwrap();
        let reply = ToolCallReply::from_lua_value(&val);
        assert!(reply.image.is_none());
        let err = reply.result.expect_err("malformed image must error");
        assert!(err.contains(expected), "got: {err}");
    }

    fn invocation(input: Value) -> LuaToolInvocation {
        let (tx, _rx) = flume::unbounded();
        LuaToolInvocation {
            tool: Arc::from("test_tool"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input,
            tx,
            permission_state: PermissionState::Ready(None),
            mutable_path_field: None,
            timeout: Some(Duration::from_secs(60)),
        }
    }

    #[test]
    fn no_header_fn_returns_tool_name() {
        let inv = invocation(serde_json::json!({"path": "/home/x/foo.rs"}));
        assert_eq!(inv.start_header().into_ready().text(), "test_tool");
    }

    fn make_lua_tool(scopes: Option<PermissionScopeKind>) -> LuaTool {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "format": { "type": "string" },
                "count": { "type": "integer" },
            },
            "required": ["url"],
        }))
        .unwrap();
        let (tx, _rx) = flume::unbounded();
        LuaTool {
            name: Arc::from("test_tool"),
            description: "test".into(),
            schema,
            audience: ToolAudience::default(),
            tx,
            plugin: Arc::from("test"),
            has_header_fn: false,
            permission: scopes.map(|scopes| ToolPermission {
                permission: Permission::FsWrite,
                scopes,
            }),
            mutable_path_field: None,
            timeout: Some(Duration::from_secs(60)),
            kind: None,
        }
    }

    #[tokio::test]
    async fn permission_scope_extracted_at_parse_time() {
        let tool = make_lua_tool(Some(PermissionScopeKind::Field(Arc::from("url"))));
        let inv = tool
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        let scopes = inv.permission_scopes().await;
        assert_eq!(
            scopes.unwrap().scopes,
            vec!["https://example.com".to_string()]
        );
    }

    #[test_case::test_case("format" ; "absent_field")]
    #[test_case::test_case("count" ; "non_string_field")]
    #[tokio::test]
    async fn permission_scope_field_invalid_forces_prompt(field: &str) {
        let input = serde_json::json!({"url": "https://example.com", "count": 42});
        let inv = make_lua_tool(Some(PermissionScopeKind::Field(Arc::from(field))))
            .parse(&input)
            .unwrap();
        let scopes = inv.permission_scopes().await.expect("should fail closed");
        assert!(scopes.force_prompt);
        assert_eq!(scopes.scopes, vec![input.to_string()]);
    }

    #[tokio::test]
    async fn permission_scope_none_when_unconfigured() {
        let unconfigured = make_lua_tool(None)
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        assert!(unconfigured.permission_scopes().await.is_none());
    }

    #[test]
    fn coerce_string_returns_ok() {
        let lua = Lua::new();
        let val = LuaValue::String(lua.create_string("hello").unwrap());
        assert_eq!(coerce_tool_result(&val), Ok("hello".to_string()));
    }

    #[test]
    fn coerce_table_with_is_error_true() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "boom").unwrap();
        t.set("is_error", true).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Err("boom".to_string())
        );
    }

    #[test]
    fn coerce_error_paths() {
        let lua = Lua::new();
        assert_eq!(
            coerce_tool_result(&LuaValue::Nil),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
        assert_eq!(
            coerce_tool_result(&LuaValue::Boolean(true)),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
        assert!(coerce_tool_result(&LuaValue::Table(lua.create_table().unwrap())).is_err());
    }

    #[test_case::test_case(LUA_FORMAT_MARKDOWN, LuaOutputFormat::Markdown ; "markdown")]
    #[test_case::test_case(LUA_FORMAT_PLAIN,    LuaOutputFormat::Plain    ; "plain")]
    #[test_case::test_case("unknown",           LuaOutputFormat::Plain    ; "unknown_defaults_to_plain")]
    fn extract_format_known_values(value: &str, expected: LuaOutputFormat) {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("format", value).unwrap();
        assert_eq!(extract_format(&t), expected);
    }

    #[test]
    fn extract_format_missing_defaults_to_plain() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        assert_eq!(extract_format(&t), LuaOutputFormat::Plain);
    }

    #[tokio::test]
    async fn needs_compute_fallback_on_failure() {
        // Closed channel → fallback to force_prompt
        let (tx, rx) = flume::bounded(0);
        drop(rx);
        let inv = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "ls"}),
            tx,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
        };
        let scopes = inv.permission_scopes().await.expect("should fallback");
        assert!(scopes.force_prompt);
        assert!(!scopes.scopes.is_empty());

        // Callback returns None → fallback to force_prompt
        let (tx2, rx2) = flume::bounded(1);
        let inv2 = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "echo hi"}),
            tx: tx2,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
        };
        std::thread::spawn(move || {
            if let Ok(Request::ComputePermissionScopes { reply, .. }) = rx2.recv() {
                let _ = reply.send(None);
            }
        });
        let scopes2 = inv2.permission_scopes().await.expect("should fallback");
        assert!(scopes2.force_prompt);
    }

    #[tokio::test]
    async fn needs_compute_returns_callback_result() {
        let (tx, rx) = flume::bounded(1);
        let inv = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "cargo test"}),
            tx,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
        };
        std::thread::spawn(move || {
            if let Ok(Request::ComputePermissionScopes { reply, .. }) = rx.recv() {
                let _ = reply.send(Some(PermissionScopes {
                    scopes: vec!["bash:cargo test".into()],
                    force_prompt: false,
                    context: craft_agent::types::PermissionContext::default(),
                }));
            }
        });
        let result = inv.permission_scopes().await;
        let scopes = result.unwrap();
        assert_eq!(scopes.scopes, vec!["bash:cargo test"]);
        assert!(!scopes.force_prompt);
    }

    #[tokio::test]
    async fn permission_scope_field_non_string_value_force_prompts() {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
            },
            "required": ["count"],
        }))
        .unwrap();
        let (tx, _rx) = flume::unbounded();
        let tool = LuaTool {
            name: Arc::from("test_tool"),
            description: "test".into(),
            schema,
            audience: ToolAudience::default(),
            tx,
            plugin: Arc::from("test"),
            has_header_fn: false,
            permission: Some(ToolPermission {
                permission: Permission::FsWrite,
                scopes: PermissionScopeKind::Field(Arc::from("count")),
            }),
            mutable_path_field: None,
            timeout: Some(Duration::from_secs(60)),
            kind: None,
        };
        let input = serde_json::json!({"count": 42});
        let inv = tool.parse(&input).unwrap();
        let scopes = inv.permission_scopes().await.expect("should fail closed");
        assert!(scopes.force_prompt);
        assert_eq!(scopes.scopes, vec![input.to_string()]);
    }

    fn timeout_spec(lua: &Lua, value: LuaValue) -> Table {
        let t = lua.create_table().unwrap();
        if !matches!(value, LuaValue::Nil) {
            t.set("timeout", value).unwrap();
        }
        t
    }

    #[test]
    fn timeout_parsing_nil_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Nil);
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_false_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Boolean(false));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_zero_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Integer(0));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_positive_seconds() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Integer(30));
        assert_eq!(parse_timeout(&spec).unwrap(), Some(Duration::from_secs(30)));
    }

    #[test_case::test_case(LuaValue::Integer(-1) ; "negative_integer")]
    #[test_case::test_case(LuaValue::Number(-1.5) ; "negative_float")]
    #[test_case::test_case(LuaValue::Boolean(true) ; "true_value")]
    fn timeout_parsing_invalid_rejected(value: LuaValue) {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, value);
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_invalid_string_rejected() {
        let lua = Lua::new();
        let s = lua.create_string("forever").unwrap();
        let spec = timeout_spec(&lua, LuaValue::String(s));
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_sub_second_float_truncates_to_zero() {
        // A sub-second float slips past `n > 0.0 && n.is_finite()`, then the
        // `n as u64` cast truncates it to 0, so the timeout fires right away.
        // Pinning this down so a future refactor does not silently change it.
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(0.5));
        assert_eq!(parse_timeout(&spec).unwrap(), Some(Duration::from_secs(0)));
    }

    #[test_case::test_case(f64::INFINITY ; "positive_infinity")]
    #[test_case::test_case(f64::NEG_INFINITY ; "negative_infinity")]
    #[test_case::test_case(f64::NAN ; "nan")]
    fn timeout_parsing_non_finite_float_rejected(n: f64) {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(n));
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_large_finite_float_accepted() {
        let lua = Lua::new();
        let big: f64 = 1e10;
        let spec = timeout_spec(&lua, LuaValue::Number(big));
        assert_eq!(
            parse_timeout(&spec).unwrap(),
            Some(Duration::from_secs(big as u64))
        );
    }

    #[test]
    fn timeout_parsing_zero_float_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(0.0));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn lua_output_format_default_is_plain() {
        assert_eq!(LuaOutputFormat::default(), LuaOutputFormat::Plain);
    }

    fn reply_table(lua: &Lua, output: &str, format: Option<&str>, is_error: bool) -> LuaValue {
        let t = lua.create_table().unwrap();
        t.set("llm_output", output).unwrap();
        if is_error {
            t.set("is_error", true).unwrap();
        }
        if let Some(f) = format {
            t.set("format", f).unwrap();
        }
        LuaValue::Table(t)
    }

    #[test]
    fn from_lua_value_table_with_markdown_format_ok() {
        let lua = Lua::new();
        let val = reply_table(&lua, "hi", Some(LUA_FORMAT_MARKDOWN), false);
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("hi".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Markdown);
    }

    #[test]
    fn from_lua_value_table_with_markdown_format_and_is_error_captures_format() {
        // The format field is read on its own, separate from is_error, so a
        // handler that fails can still ask for its error message to be rendered
        // as markdown.
        let lua = Lua::new();
        let val = reply_table(&lua, "boom", Some(LUA_FORMAT_MARKDOWN), true);
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Err("boom".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Markdown);
    }

    #[test]
    fn from_lua_value_table_without_format_defaults_to_plain() {
        let lua = Lua::new();
        let val = reply_table(&lua, "hi", None, false);
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("hi".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn from_lua_value_string_value_defaults_to_plain() {
        let lua = Lua::new();
        let val = LuaValue::String(lua.create_string("hello").unwrap());
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("hello".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
        assert!(reply.snapshot.is_none());
        assert!(reply.live_buf.is_none());
        assert!(reply.header.is_none());
    }

    #[test]
    fn from_lua_value_non_table_non_string_is_err_with_default_format() {
        let reply = ToolCallReply::from_lua_value(&LuaValue::Boolean(true));
        assert_eq!(reply.result, Err(TOOL_HANDLER_RETURN_ERR.to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn coerce_table_with_is_error_false_returns_ok() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "fine").unwrap();
        t.set("is_error", false).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Ok("fine".to_string())
        );
    }

    #[test]
    fn coerce_table_with_non_string_output_is_err() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", 123).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
    }

    #[test_case::test_case("_leading", true ; "leading_underscore_allowed")]
    #[test_case::test_case("_", true ; "single_underscore")]
    #[test_case::test_case("snake_case_123", true ; "snake_with_digits")]
    #[test_case::test_case("foo-bar", false ; "hyphen_rejected")]
    #[test_case::test_case("foo.bar", false ; "dot_rejected")]
    #[test_case::test_case("foo@bar", false ; "at_sign_rejected")]
    #[test_case::test_case("café", false ; "non_ascii_rejected")]
    #[test_case::test_case("名前", false ; "unicode_rejected")]
    fn tool_name_validation_extra(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
    }

    #[test]
    fn tool_name_validation_length_boundaries() {
        let max_ok: String = "a".repeat(TOOL_NAME_MAX);
        assert!(is_valid_tool_name(&max_ok));
        let too_long: String = "a".repeat(TOOL_NAME_MAX + 1);
        assert!(!is_valid_tool_name(&too_long));
    }
}
