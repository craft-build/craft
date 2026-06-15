//! Lifecycle hook bridge: implements `craft_agent::Hooks` by dispatching into the
//! single-threaded Lua VM via the request channel. Registration happens through
//! `craft.api.register_hook`; this module wires the runtime side.

use std::sync::Arc;

use mlua::{Function, RegistryKey, Value as LuaValue};
use serde_json::Value;
use tracing::warn;

use craft_agent::hooks::{HookDecision, HookFuture, Hooks, ToolUseEvent};

use crate::loader::EventHandle;
use crate::runtime::Request;

/// Event names accepted by `craft.api.register_hook`.
pub const EVENT_SESSION_START: &str = "session_start";
pub const EVENT_PRE_TOOL_USE: &str = "pre_tool_use";
pub const EVENT_POST_TOOL_USE: &str = "post_tool_use";

/// `app_data` map: event name -> registered handlers. Each handler is a Lua
/// function stored as a `RegistryKey`, plus the owning plugin name.
pub type HookHandlerMap = std::collections::HashMap<String, Vec<(Arc<str>, RegistryKey)>>;

pub struct LuaHooks {
    handle: EventHandle,
}

impl LuaHooks {
    pub fn new(handle: EventHandle) -> Arc<Self> {
        Arc::new(Self { handle })
    }
}

async fn fire_session_start(handle: &EventHandle) {
    let (tx, rx) = flume::bounded(1);
    let _ = handle.tx().send(Request::RunHook {
        event: EVENT_SESSION_START.to_string(),
        tool: String::new(),
        input: Value::Null,
        output: String::new(),
        is_error: false,
        reply: tx,
    });
    let _ = rx.recv_async().await;
}

async fn fire_pre_tool_use(handle: &EventHandle, tool: String, input: Value) -> HookDecision {
    let (tx, rx) = flume::bounded(1);
    let _ = handle.tx().send(Request::RunHook {
        event: EVENT_PRE_TOOL_USE.to_string(),
        tool,
        input,
        output: String::new(),
        is_error: false,
        reply: tx,
    });
    rx.recv_async().await.map(|r| r.decision).unwrap_or(HookDecision::Allow)
}

async fn fire_post_tool_use(
    handle: &EventHandle,
    tool: String,
    input: Value,
    output: String,
    is_error: bool,
) {
    let (tx, rx) = flume::bounded(1);
    let _ = handle.tx().send(Request::RunHook {
        event: EVENT_POST_TOOL_USE.to_string(),
        tool,
        input,
        output,
        is_error,
        reply: tx,
    });
    let _ = rx.recv_async().await;
}

impl Hooks for LuaHooks {
    fn session_start(&self) -> HookFuture<'_, ()> {
        let handle = self.handle.clone();
        Box::pin(async move {
            fire_session_start(&handle).await;
        })
    }

    fn pre_tool_use(&self, event: ToolUseEvent) -> HookFuture<'_, HookDecision> {
        let handle = self.handle.clone();
        Box::pin(async move { fire_pre_tool_use(&handle, event.tool, event.input).await })
    }

    fn post_tool_use(
        &self,
        event: ToolUseEvent,
        output: String,
        is_error: bool,
    ) -> HookFuture<'_, ()> {
        let handle = self.handle.clone();
        Box::pin(async move {
            fire_post_tool_use(&handle, event.tool, event.input, output, is_error).await;
        })
    }
}

/// Reply from the VM thread after running hook handlers.
pub struct HookReply {
    pub decision: HookDecision,
}

/// Runs inside the VM thread (single-threaded). Invokes every registered handler
/// for the given event in registration order. For `pre_tool_use`, the first
/// non-`allow` decision wins; subsequent handlers are still called for side effects
/// unless a deny short-circuits.
pub async fn run_hooks_in_vm(
    lua: &mlua::Lua,
    event: &str,
    tool: &str,
    input: &Value,
    output: &str,
    is_error: bool,
) -> HookReply {
    let handlers: Vec<(Arc<str>, Function)> = {
        let Some(map) = lua.app_data_ref::<HookHandlerMap>() else {
            return HookReply {
                decision: HookDecision::Allow,
            };
        };
        let Some(regs) = map.get(event) else {
            return HookReply {
                decision: HookDecision::Allow,
            };
        };
        regs
            .iter()
            .filter_map(|(plugin, key)| {
                lua.registry_value::<Function>(key)
                    .ok()
                    .map(|f| (Arc::clone(plugin), f))
            })
            .collect()
    };

    if handlers.is_empty() {
        return HookReply {
            decision: HookDecision::Allow,
        };
    }

    let mut decision = HookDecision::Allow;

    for (plugin, func) in handlers {
        let lua_input = json_to_lua_value(lua, input);

        let result: mlua::Result<LuaValue> =
            crate::runtime::run_detached(lua, async move {
                let thread = lua.create_thread(func)?;
                let args = build_hook_args(lua, event, tool, lua_input, output, is_error)?;
                thread.into_async::<LuaValue>(args)?.await
            })
            .await;

        match result {
            Ok(ret) => {
                if event == EVENT_PRE_TOOL_USE {
                    if matches!(decision, HookDecision::Allow) {
                        decision = parse_pre_decision(&ret);
                    }
                    if matches!(decision, HookDecision::Deny { .. }) {
                        break;
                    }
                }
            }
            Err(e) => {
                warn!(plugin = %plugin, event, error = %e, "hook handler failed");
            }
        }
    }

    HookReply { decision }
}

fn build_hook_args(
    lua: &mlua::Lua,
    event: &str,
    tool: &str,
    input: LuaValue,
    output: &str,
    is_error: bool,
) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    let _ = t.set("event", event);
    let _ = t.set("tool", tool);
    let _ = t.set("input", input);
    if event == EVENT_POST_TOOL_USE {
        let _ = t.set("output", output);
        let _ = t.set("is_error", is_error);
    }
    Ok(t)
}

fn parse_pre_decision(ret: &LuaValue) -> HookDecision {
    match ret {
        LuaValue::Nil | LuaValue::Boolean(true) => HookDecision::Allow,
        LuaValue::String(s) => {
            let action = s.to_string_lossy().to_string();
            if action == "allow" {
                HookDecision::Allow
            } else {
                HookDecision::Deny {
                    message: "blocked by hook".into(),
                }
            }
        }
        LuaValue::Table(t) => {
            let action: String = t.get("action").unwrap_or_default();
            match action.as_str() {
                "deny" => HookDecision::Deny {
                    message: t
                        .get("message")
                        .unwrap_or_else(|_| "blocked by hook".into()),
                },
                "transform" => {
                    let new_input: Value = lua_value_to_json(&t.get("input").unwrap_or(LuaValue::Nil));
                    HookDecision::Transform { input: new_input }
                }
                _ => HookDecision::Allow,
            }
        }
        _ => HookDecision::Allow,
    }
}

fn lua_value_to_json(val: &LuaValue) -> Value {
    match val {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => Value::Bool(*b),
        LuaValue::Integer(i) => Value::from(*i),
        LuaValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        LuaValue::String(s) => Value::String(s.to_string_lossy().to_string()),
        LuaValue::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t.clone().pairs::<String, LuaValue>().flatten() {
                map.insert(k, lua_value_to_json(&v));
            }
            Value::Object(map)
        }
        _ => Value::Null,
    }
}

fn json_to_lua_value(lua: &mlua::Lua, val: &Value) -> LuaValue {
    match val {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                LuaValue::Integer(i)
            } else {
                LuaValue::Number(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => match lua.create_string(s) {
            Ok(s) => LuaValue::String(s),
            Err(_) => LuaValue::Nil,
        },
        Value::Object(map) => match lua.create_table() {
            Ok(t) => {
                for (k, v) in map {
                    let _ = t.set(k.as_str(), json_to_lua_value(lua, v));
                }
                LuaValue::Table(t)
            }
            Err(_) => LuaValue::Nil,
        },
        Value::Array(arr) => match lua.create_table() {
            Ok(t) => {
                for (i, v) in arr.iter().enumerate() {
                    let _ = t.set(i + 1, json_to_lua_value(lua, v));
                }
                LuaValue::Table(t)
            }
            Err(_) => LuaValue::Nil,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allow_from_nil() {
        let lua = mlua::Lua::new();
        assert!(matches!(
            parse_pre_decision(&LuaValue::Nil),
            HookDecision::Allow
        ));
        let _ = lua;
    }

    #[test]
    fn parse_deny_from_table() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("action", "deny").unwrap();
        t.set("message", "nope").unwrap();
        match parse_pre_decision(&LuaValue::Table(t)) {
            HookDecision::Deny { message } => assert_eq!(message, "nope"),
            other => panic!("expected deny, got {other:?}"),
        }
    }
}
