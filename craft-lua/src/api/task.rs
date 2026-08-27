//! `craft.task`: the subagents of the focused session and their transcripts.
//! The host keeps the subagent transcripts, so a plugin only needs to list the
//! tasks and show one.

use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::command::{TaskRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::Pair;

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: TaskRequest,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Task {
        req,
        reply_tx,
    })
    .await
}

/// Lists the focused session's chats in chat order. Entry 1 is always the main
/// chat, with id `"main"` and no `status`: its work is the session's own, and
/// `craft.session.live()` already reports that. The rest are subagents, keyed
/// by the tool call that spawned them.
///
/// Returns an array of `{id, name, focused, status?}` where `status` is
/// `"working"`, `"done"`, or `"error"`, or nil and an error.
pub(crate) fn create_task_table(
    lua: &Lua,
    tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "list",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, ()| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, TaskRequest::List).await }
            }
        })?,
    )?;

    t.set(
        "focus",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, id: String| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, TaskRequest::Focus { id }).await }
            }
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use serde_json::json;
    use std::thread::JoinHandle;
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const DONE_ID: &str = "toolu_02";
    const ERROR_ID: &str = "toolu_03";
    const MAIN_ID: &str = "main";
    const MAIN_NAME: &str = "chat";
    const SUBAGENT_NAME: &str = "explore repo";
    const WORKING_STATUS: &str = "working";
    const DONE_STATUS: &str = "done";
    const ERROR_STATUS: &str = "error";
    const NIL_STATUS: &str = "nil";
    const UNKNOWN_TASK_ERR: &str = "unknown task: toolu_01";
    const NO_REQUEST_ERR: &str = "expected a task request";
    const LIST_CALL: &str = "return task.list()";
    const FOCUS_CALL: &str = "return task.focus('toolu_01')";
    const LIST_SCRIPT: &str = "
        local tasks = task.list()
        local ids, statuses = {}, {}
        for i, t in ipairs(tasks) do
            ids[i] = t.id
            statuses[i] = tostring(t.status)
        end
        return table.concat(ids, ','), table.concat(statuses, ','), tasks[1].name
    ";

    fn lua_with_task(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_task_table(&lua, tx).unwrap();
        lua.globals().set("task", t).unwrap();
        lua
    }

    /// Answers the first request with {reply} and hands it back to assert on.
    /// Join only after dropping the `Lua` that holds the sender, or a missing
    /// request parks the thread forever.
    fn spawn_host(rx: flume::Receiver<UiAction>, reply: UiReply) -> JoinHandle<TaskRequest> {
        std::thread::spawn(move || {
            let Ok(UiAction::Task { req, reply_tx }) = rx.recv() else {
                panic!("{NO_REQUEST_ERR}");
            };
            reply_tx.send(reply).unwrap();
            req
        })
    }

    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[tokio::test]
    async fn without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_task(None);
        let (val, err): (Value, Option<String>) = lua.load(code).eval_async().await.unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// A stale id should be visible but not fatal: the host's `Err` comes back
    /// as the `(nil, err)` pair, word for word, without raising into the plugin.
    #[test_case(LIST_CALL ; "list")]
    #[test_case(FOCUS_CALL ; "focus")]
    #[tokio::test]
    async fn host_error_reply_surfaces_as_error_pair(code: &str) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(rx, Err(UNKNOWN_TASK_ERR.to_owned()));

        let (val, err): (Value, Option<String>) = lua
            .load(code)
            .eval_async()
            .await
            .expect("error reply must not throw");
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(UNKNOWN_TASK_ERR));

        drop(lua);
        host.join().unwrap();
    }

    /// `plugins/task/picker_rows.lua` spots the main chat with `if not task.status`,
    /// so a missing key has to stay missing on the Lua side.
    #[tokio::test]
    async fn list_passes_host_array_through_unchanged() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let host = spawn_host(
            rx,
            Ok(json!([
                { "id": MAIN_ID, "name": MAIN_NAME, "focused": true },
                { "id": TASK_ID, "name": SUBAGENT_NAME, "status": WORKING_STATUS, "focused": false },
                { "id": DONE_ID, "name": SUBAGENT_NAME, "status": DONE_STATUS, "focused": false },
                { "id": ERROR_ID, "name": SUBAGENT_NAME, "status": ERROR_STATUS, "focused": false },
            ])),
        );

        let (ids, statuses, main_name): (String, String, String) =
            lua.load(LIST_SCRIPT).eval_async().await.unwrap();
        assert_eq!(ids, format!("{MAIN_ID},{TASK_ID},{DONE_ID},{ERROR_ID}"));
        assert_eq!(
            statuses,
            format!("{NIL_STATUS},{WORKING_STATUS},{DONE_STATUS},{ERROR_STATUS}")
        );
        assert_eq!(main_name, MAIN_NAME);

        drop(lua);
        assert!(matches!(host.join().unwrap(), TaskRequest::List));
    }

    #[tokio::test]
    async fn focus_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Task {
                req: TaskRequest::Focus { id },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) = lua
            .load(format!("return task.focus('{TASK_ID}')"))
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), TASK_ID);
    }

    #[test]
    fn create_task_table_exposes_only_list_and_focus() {
        let lua = Lua::new();
        let t = create_task_table(&lua, None).unwrap();
        let mut names: Vec<String> = t
            .pairs::<String, Value>()
            .filter_map(|e| e.ok().map(|(k, _)| k))
            .collect();
        names.sort();
        assert_eq!(names, vec!["focus", "list"]);
    }
}
