use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{Function, Lua, Result as LuaResult, Table, Value};

use crate::api::util::dispatch::DepthGuard;
use crate::runtime::{run_detached, strip_traceback};

static NEXT_AUTOCMD_ID: AtomicU64 = AtomicU64::new(1);

const WILDCARD_PATTERN: &str = "*";

pub(crate) struct AutocmdEntry {
    pub id: u64,
    pub callback: Function,
    pub plugin: Arc<str>,
    pub once: bool,
    pub patterns: Option<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct AutocmdStore {
    pub(crate) listeners: HashMap<String, Vec<AutocmdEntry>>,
}

impl AutocmdStore {
    pub fn register(&mut self, event: String, entry: AutocmdEntry) {
        self.listeners.entry(event).or_default().push(entry);
    }

    pub fn remove(&mut self, id: u64) {
        for entries in self.listeners.values_mut() {
            entries.retain(|e| e.id != id);
        }
        self.listeners.retain(|_, v| !v.is_empty());
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        for entries in self.listeners.values_mut() {
            entries.retain(|e| e.plugin.as_ref() != plugin);
        }
        self.listeners.retain(|_, v| !v.is_empty());
    }
}

fn pattern_matches(patterns: Option<&[String]>, fired: Option<&str>) -> bool {
    match patterns {
        None => true,
        Some(ps) => {
            ps.iter().any(|p| p == WILDCARD_PATTERN)
                || fired.is_some_and(|f| ps.iter().any(|p| p == f))
        }
    }
}

/// One dispatch path for host-fired and plugin-fired events. Never throws.
///
/// Each callback runs in its own coroutine under a detached task scope, so
/// it may suspend (the `craft.fs.*` helpers park on blocking IO); an inline
/// resume would die with "attempt to yield across metamethod / C-call
/// boundary". The per-callback scope also means jobs a handler starts die
/// with that handler instead of outliving it to the end of the batch.
///
/// The snapshot below looks racy but is not: all Lua runs on the runtime
/// thread and plugin unloads arrive through the request channel, so nothing
/// can unload between the snapshot and the calls.
pub(crate) async fn dispatch(lua: Lua, event: String, pattern: Option<String>, data: Value) {
    let Ok(_guard) = DepthGuard::enter(&lua, "autocmd", &event) else {
        tracing::warn!(event, "autocmd dispatch exceeded max depth, skipping");
        return;
    };
    let snapshot: Vec<(u64, Arc<str>, Function)> = {
        let Some(mut store) = lua.app_data_mut::<AutocmdStore>() else {
            return;
        };
        let Some(entries) = store.listeners.get_mut(&event) else {
            return;
        };
        let mut snapshot = Vec::new();
        // Drop `once` entries now, at snapshot time: if a callback refires
        // the same event they are already gone, so they stay exactly-once.
        entries.retain(|e| {
            let fires = pattern_matches(e.patterns.as_deref(), pattern.as_deref());
            if fires {
                snapshot.push((e.id, Arc::clone(&e.plugin), e.callback.clone()));
            }
            !(fires && e.once)
        });
        snapshot
    };
    for (id, plugin, callback) in snapshot {
        let ev = match make_ev_table(&lua, id, &event, pattern.as_deref(), &data) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(event, error = %e, "failed to build autocmd ev table");
                return;
            }
        };
        if let Err(e) = run_detached(&lua, async {
            let thread = lua.create_thread(callback)?;
            thread.into_async::<()>(ev)?.await
        })
        .await
        {
            tracing::warn!(
                event,
                plugin = &*plugin,
                error = %strip_traceback(&e),
                "plugin callback failed"
            );
        }
    }
}

fn make_ev_table(
    lua: &Lua,
    id: u64,
    event: &str,
    pattern: Option<&str>,
    data: &Value,
) -> LuaResult<Table> {
    let ev = lua.create_table()?;
    ev.set("id", id)?;
    ev.set("event", event)?;
    ev.set("match", pattern)?;
    ev.set("data", data.clone())?;
    Ok(ev)
}

fn parse_string_or_seq(value: Value, what: &str) -> LuaResult<Vec<String>> {
    match value {
        Value::String(s) => Ok(vec![s.to_str()?.to_owned()]),
        Value::Table(t) => t.sequence_values::<String>().collect(),
        _ => Err(mlua::Error::runtime(format!(
            "{what} must be a string or string[]"
        ))),
    }
}

pub(crate) fn add_autocmd_methods(api_table: &Table, lua: &Lua, plugin: Arc<str>) -> LuaResult<()> {
    let p = Arc::clone(&plugin);
    api_table.set(
        "create_autocmd",
        lua.create_function(move |lua, (event, opts): (Value, Table)| {
            let events = parse_string_or_seq(event, "event")?;
            let callback: Function = opts.get("callback")?;
            let once: bool = opts.get("once").unwrap_or(false);
            let patterns = match opts.get::<Value>("pattern")? {
                Value::Nil => None,
                v => Some(parse_string_or_seq(v, "pattern")?),
            };
            let id = NEXT_AUTOCMD_ID.fetch_add(1, Ordering::Relaxed);
            let mut store = lua
                .app_data_mut::<AutocmdStore>()
                .ok_or_else(|| mlua::Error::runtime("autocmd store not initialized"))?;
            for event in events {
                store.register(
                    event,
                    AutocmdEntry {
                        id,
                        callback: callback.clone(),
                        plugin: Arc::clone(&p),
                        once,
                        patterns: patterns.clone(),
                    },
                );
            }
            Ok(id)
        })?,
    )?;

    api_table.set(
        "del_autocmd",
        lua.create_function(|lua, id: u64| {
            if let Some(mut store) = lua.app_data_mut::<AutocmdStore>() {
                store.remove(id);
            }
            Ok(())
        })?,
    )?;

    // A handler may suspend, so this call may too. That rules it out inside
    // a slot chain, which runs synchronously (see `declare_slot`): the first
    // handler to park dies with "attempt to yield across metamethod/C-call
    // boundary". Fire the event from the code that calls the slot instead.
    api_table.set(
        "exec_autocmds",
        lua.create_async_function(
            |lua: Lua, (event, opts): (Value, Option<Table>)| async move {
                let events = parse_string_or_seq(event, "event")?;
                let (pattern, data) = match opts {
                    Some(opts) => {
                        let pattern = match opts.get::<Value>("pattern")? {
                            Value::Nil => None,
                            Value::String(s) => Some(s.to_str()?.to_owned()),
                            _ => return Err(mlua::Error::runtime("pattern must be a string")),
                        };
                        (pattern, opts.get::<Value>("data")?)
                    }
                    None => (None, Value::Nil),
                };
                for event in events {
                    dispatch(lua.clone(), event, pattern.clone(), data.clone()).await;
                }
                Ok(())
            },
        )?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(None, None => true ; "no_patterns_no_fired")]
    #[test_case(None, Some("x") => true ; "no_patterns_with_fired")]
    #[test_case(Some(&["*"]), None => true ; "wildcard_no_fired")]
    #[test_case(Some(&["a", "*"]), Some("z") => true ; "wildcard_among_others")]
    #[test_case(Some(&["a", "b"]), Some("b") => true ; "fired_in_patterns")]
    #[test_case(Some(&["a", "b"]), Some("c") => false ; "fired_not_in_patterns")]
    #[test_case(Some(&["a"]), None => false ; "patterns_but_no_fired")]
    fn match_rule(patterns: Option<&[&str]>, fired: Option<&str>) -> bool {
        let owned = patterns.map(|ps| ps.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        pattern_matches(owned.as_deref(), fired)
    }
}
