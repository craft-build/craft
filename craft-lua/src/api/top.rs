//! Top-level entries directly on the `craft` global: `defer_fn` (a
//! UI-scoped timer for "run this after N ms, not tied to my task") and
//! `notify` (a one-line notice whose default any plugin can swap via
//! `craft.set_notify_handler`).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mlua::{
    Function, Lua, RegistryKey, Result as LuaResult, Table, UserData, UserDataMethods, Value,
};

use crate::api::util::command::{UiAction, ui_send};
use crate::runtime::{DeferQueue, DeferredCallback};

/// The one `craft.notify` override for the whole process. `create_craft_global`
/// runs per plugin load, so an override stored on the `craft` table itself would
/// only ever reach the plugin that installed it. The owner's name rides along
/// so [`clear_notify_handler`] can hand the slot back when that plugin is
/// unloaded, instead of leaving everyone calling into a dead env.
#[derive(Default)]
pub(crate) struct NotifyHandler(pub(crate) Mutex<Option<(Arc<str>, RegistryKey)>>);

/// Handle returned by `craft.defer_fn`. The runtime reads the flag once the
/// timer goes off, so flipping it before then skips the callback entirely.
pub(crate) struct Timer {
    cancel: Arc<AtomicBool>,
}

impl UserData for Timer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Cancel the pending callback. Safe to call more than once, and does
        // nothing once the callback has already run.
        methods.add_method("stop", |_, this, ()| {
            this.cancel.store(true, Ordering::Release);
            Ok(())
        });
    }
}

/// Run {callback} after {ms} milliseconds, on the Lua thread and outside
/// any task scope. The timer does not hang off the caller's cancel token
/// or the `async.run` deadline, so the callback still fires once the tool
/// call that scheduled it is over. That is what a toast needs to dismiss
/// itself, and the difference from `craft.async.sleep`.
///
/// You get back a handle. Its `:stop()` cancels a callback that has not
/// fired yet, which is how you debounce: schedule, then stop and
/// reschedule on every new event. An error raised by the callback is
/// logged and dropped, since nobody is waiting for a result.
fn defer_fn(lua: &Lua, plugin: Arc<str>, callback: Function, ms: u64) -> LuaResult<Timer> {
    let queue = lua
        .app_data_ref::<DeferQueue>()
        .ok_or_else(|| mlua::Error::runtime("defer queue not initialized"))?;
    let func = lua.create_registry_value(callback)?;
    let cancel = Arc::new(AtomicBool::new(false));
    if let Err(rejected) = queue.push(DeferredCallback {
        func,
        delay: Duration::from_millis(ms),
        plugin,
        cancel: Arc::clone(&cancel),
    }) {
        // Reclaim the registry slot, the runtime never got the callback.
        let _ = lua.remove_registry_value(rejected.func);
        return Err(mlua::Error::runtime("defer queue closed"));
    }
    Ok(Timer { cancel })
}

/// Show a one line notice. By default it goes to `craft.ui.flash`, with
/// `{opts.title}` in front of the message when you pass one. A run with
/// no UI, such as a headless run, logs the notice instead of dropping it.
///
/// There is one handler for the whole process. Once a plugin calls
/// `craft.set_notify_handler`, notices from every plugin go through it.
/// That is how a UI plugin turns flashes into stacked toasts without
/// any of the callers knowing about it.
///
/// {level} reaches the handler untouched, and the default ignores it.
fn notify(
    lua: &Lua,
    tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
    msg: String,
    level: Option<String>,
    opts: Option<Table>,
) -> LuaResult<()> {
    let title = match &opts {
        Some(t) => t.get::<Option<String>>("title")?,
        None => None,
    };
    let handler_fn = lua.app_data_ref::<NotifyHandler>().and_then(|slot| {
        let guard = locked(&slot);
        let (_, key) = guard.as_ref()?;
        lua.registry_value::<Function>(key).ok()
    });
    if let Some(func) = handler_fn {
        // A broken override must not swallow the message, so fall through.
        match func.call::<()>((msg.clone(), level.clone(), opts)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "craft.notify handler failed; falling through to flash");
            }
        }
    }
    let text = match title {
        Some(title) => format!("{title}: {msg}"),
        None => msg,
    };
    if ui_send(tx.as_ref(), UiAction::Flash(text.clone())).is_err() {
        tracing::info!(plugin = %plugin, level = level.as_deref(), "{text}");
    }
    Ok(())
}

/// Install the handler that every `craft.notify` call in the process goes
/// through, in place of the default flash. Pass `nil` to put the default
/// back.
///
/// The handler runs on the Lua thread, so keep it short and hand real
/// work to `craft.async.run`. If it raises an error, the error is logged
/// and the notice falls back to `craft.ui.flash`, so the user still sees
/// it. Unloading the plugin that installed the handler also restores the
/// default.
fn set_notify_handler(lua: &Lua, plugin: Arc<str>, handler: Value) -> LuaResult<()> {
    install_notify_handler(lua, plugin, handler)
}

/// Install a `craft.notify` override into the shared slot. Reached both by
/// `set_notify_handler` and by the `craft.notify = fn` sugar that the `craft`
/// global's `__newindex` routes here.
pub(crate) fn install_notify_handler(lua: &Lua, plugin: Arc<str>, handler: Value) -> LuaResult<()> {
    // Validate the argument before touching the slot: a wrong-typed
    // handler must not silently clear a previously installed one.
    let new_key = match handler {
        Value::Nil => None,
        Value::Function(f) => Some((plugin, lua.create_registry_value(f)?)),
        _ => {
            return Err(mlua::Error::runtime(
                "set_notify_handler expects a function or nil",
            ));
        }
    };
    let slot = lua
        .app_data_ref::<NotifyHandler>()
        .ok_or_else(|| mlua::Error::runtime("notify handler slot not initialized"))?;
    let old = std::mem::replace(&mut *locked(&slot), new_key);
    if let Some((_, key)) = old {
        let _ = lua.remove_registry_value(key);
    }
    Ok(())
}

fn locked(slot: &NotifyHandler) -> std::sync::MutexGuard<'_, Option<(Arc<str>, RegistryKey)>> {
    slot.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hand the slot back when {plugin} is unloaded. Its handler closes over an
/// env that is going away, and nobody else knows to clean up after it.
pub(crate) fn clear_notify_handler(lua: &Lua, plugin: &str) {
    let Some(slot) = lua.app_data_ref::<NotifyHandler>() else {
        return;
    };
    if let Some((_, key)) = locked(&slot).take_if(|(owner, _)| &**owner == plugin) {
        let _ = lua.remove_registry_value(key);
    }
}

pub(crate) fn add_top_methods(craft: &Table, lua: &Lua, plugin: Arc<str>) -> LuaResult<()> {
    let defer_plugin = Arc::clone(&plugin);
    craft.set(
        "defer_fn",
        lua.create_function(move |lua, (callback, ms): (Function, u64)| {
            defer_fn(lua, Arc::clone(&defer_plugin), callback, ms)
        })?,
    )?;

    let notify_plugin = Arc::clone(&plugin);
    craft.set(
        "set_notify_handler",
        lua.create_function(move |lua, handler: Value| {
            set_notify_handler(lua, Arc::clone(&notify_plugin), handler)
        })?,
    )?;

    Ok(())
}

/// The `notify` function itself, kept off the raw `craft` table so the
/// metatable's `__newindex` can catch a `craft.notify = fn` assignment and
/// route it into the shared slot.
pub(crate) fn notify_function(
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
) -> LuaResult<Function> {
    let tx = ui_action_tx;
    lua.create_function(
        move |lua, (msg, level, opts): (String, Option<String>, Option<Table>)| {
            notify(lua, tx.clone(), Arc::clone(&plugin), msg, level, opts)
        },
    )
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const BY_FUNCTION: &str = "craft.set_notify_handler(function(msg) seen = msg end)";
    const BY_ASSIGNMENT: &str = "craft.notify = function(msg) seen = msg end";

    /// Builds one plugin's `craft` table under the global {plugin}, wired the
    /// way `create_craft_global` wires it: `notify` on the metatable's
    /// `__index` so `craft.notify = fn` hits `__newindex` and lands in the
    /// shared slot instead of shadowing it.
    fn install(lua: &Lua, tx: Option<flume::Sender<UiAction>>, plugin: &str) {
        if lua.app_data_ref::<NotifyHandler>().is_none() {
            lua.set_app_data(NotifyHandler::default());
        }
        let craft = lua.create_table().unwrap();
        let owner: Arc<str> = Arc::from(plugin);
        add_top_methods(&craft, lua, Arc::clone(&owner)).unwrap();
        let index = lua.create_table().unwrap();
        index
            .set(
                "notify",
                notify_function(lua, tx, Arc::clone(&owner)).unwrap(),
            )
            .unwrap();
        let router = lua
            .create_function(
                move |lua, (t, k, v): (Table, String, Value)| match k.as_str() {
                    "notify" => install_notify_handler(lua, Arc::clone(&owner), v),
                    _ => t.raw_set(k, v),
                },
            )
            .unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("__index", index).unwrap();
        meta.set("__newindex", router).unwrap();
        craft.set_metatable(Some(meta)).unwrap();
        lua.globals().set(plugin, craft).unwrap();
    }

    fn flashed(rx: &flume::Receiver<UiAction>) -> String {
        match rx.try_recv().expect("a flash") {
            UiAction::Flash(msg) => msg,
            _ => panic!("expected Flash"),
        }
    }

    #[test_case("" ; "no handler installed")]
    #[test_case("craft.set_notify_handler(function() end) craft.set_notify_handler(nil)" ; "handler removed again")]
    fn notify_falls_back_to_flash(prelude: &str) {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx), "craft");
        lua.load(format!(r#"{prelude} craft.notify("hi")"#))
            .exec()
            .unwrap();
        assert_eq!(flashed(&rx), "hi");
    }

    #[test]
    fn notify_title_labels_the_flash() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx), "craft");
        lua.load(r#"craft.notify("hi", nil, { title = "make" })"#)
            .exec()
            .unwrap();
        assert_eq!(flashed(&rx), "make: hi");
    }

    /// Headless runs have no UI to flash to, and a plugin should not have to
    /// care.
    #[test]
    fn notify_without_a_ui_is_not_an_error() {
        let lua = Lua::new();
        install(&lua, None, "craft");
        lua.load(r#"craft.notify("hi", "warn")"#).exec().unwrap();
    }

    /// Both spellings of "override notify" have to reach notify calls made
    /// through *any* plugin's `craft`, not just the installer's. That is the
    /// whole point of a shared slot.
    #[test_case(BY_FUNCTION ; "set_notify_handler")]
    #[test_case(BY_ASSIGNMENT ; "assignment")]
    fn override_reroutes_every_plugins_notify(install_handler: &str) {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded();
        install(&lua, Some(tx.clone()), "craft");
        install(&lua, Some(tx), "craft_b");

        let seen: String = lua
            .load(format!(
                r#"
                seen = ""
                {install_handler}
                craft_b.notify("from the other plugin")
                return seen
            "#
            ))
            .eval()
            .unwrap();
        assert_eq!(seen, "from the other plugin");
    }

    /// The handler closes over an env that dies with its plugin, so unloading
    /// has to hand the slot back rather than leave everyone calling into it.
    #[test]
    fn unloading_the_installer_restores_the_default() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx.clone()), "craft");
        install(&lua, Some(tx), "craft_b");
        lua.load(r#"seen = "" craft_b.set_notify_handler(function(msg) seen = msg end)"#)
            .exec()
            .unwrap();

        clear_notify_handler(&lua, "craft");
        lua.load(r#"craft.notify("still routed")"#).exec().unwrap();
        assert_eq!(lua.globals().get::<String>("seen").unwrap(), "still routed");
        assert!(rx.try_recv().is_err(), "the handler took it, not the flash");

        clear_notify_handler(&lua, "craft_b");
        lua.load(r#"craft.notify("back to flash")"#).exec().unwrap();
        assert_eq!(flashed(&rx), "back to flash");
    }

    #[test]
    fn notify_assignment_rejects_non_function() {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded();
        install(&lua, Some(tx), "craft");
        let err = lua
            .load(r#"craft.notify = 42"#)
            .exec()
            .expect_err("assigning a non-function to craft.notify must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("function or nil"),
            "expected type error, got: {msg}"
        );
    }

    /// The sleep-and-dispatch half lives in the runtime, so this only pins
    /// the contract between the handle and the flag the runtime reads.
    #[test]
    fn defer_handle_stop_marks_cancel_flag() {
        let lua = Lua::new();
        lua.set_app_data(DeferQueue::new());
        install(&lua, None, "craft");
        lua.load(r#"H = craft.defer_fn(function() end, 5000)"#)
            .exec()
            .unwrap();
        let cancel = {
            let queue = lua.app_data_ref::<DeferQueue>().unwrap();
            let cb = queue.rx.try_recv().expect("callback queued");
            Arc::clone(&cb.cancel)
        };
        assert!(!cancel.load(Ordering::Acquire));
        lua.load(r#"H:stop()"#).exec().unwrap();
        assert!(cancel.load(Ordering::Acquire));
    }
}
