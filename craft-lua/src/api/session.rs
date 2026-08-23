//! `craft.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.

use craft_agent::SessionMailbox;
use craft_storage::id::CraftId;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::util::command::{SessionRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::{Pair, err_pair};

const BLANK_NOTIFY_ERR: &str = "text must not be blank";
const SESSION_REQUIRED_ERR: &str = "session is required";

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair<mlua::Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Session {
        req,
        reply_tx,
    })
    .await
}

pub(crate) fn create_session_table(
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
                async move { roundtrip(lua, tx, SessionRequest::List).await }
            }
        })?,
    )?;

    t.set(
        "live",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, ()| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, SessionRequest::Live).await }
            }
        })?,
    )?;

    t.set(
        "current",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, ()| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, SessionRequest::Current).await }
            }
        })?,
    )?;

    t.set(
        "focus",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, id: String| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, SessionRequest::Focus { id }).await }
            }
        })?,
    )?;

    t.set(
        "delete",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, id: String| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, SessionRequest::Delete { id }).await }
            }
        })?,
    )?;

    t.set(
        "new",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, opts: Option<Table>| {
                let tx = tx.clone();
                async move {
                    let (prompt, focus) = match opts {
                        Some(opts) => (opts.get("prompt")?, opts.get("focus").unwrap_or(false)),
                        None => (None, false),
                    };
                    roundtrip(lua, tx, SessionRequest::New { prompt, focus }).await
                }
            }
        })?,
    )?;

    t.set(
        "prompt",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, (text, opts): (String, Option<Table>)| {
                let tx = tx.clone();
                async move {
                    let id = match opts {
                        Some(opts) => opts.get("session")?,
                        None => None,
                    };
                    roundtrip(lua, tx, SessionRequest::Prompt { id, text }).await
                }
            }
        })?,
    )?;

    t.set(
        "notify",
        lua.create_function(move |_, (text, opts): (String, Option<Table>)| {
            if text.trim().is_empty() {
                return Ok(err_pair::<bool>(BLANK_NOTIFY_ERR));
            }
            let Some(opts) = opts else {
                return Ok(err_pair::<bool>(SESSION_REQUIRED_ERR));
            };
            let Some(raw_id): Option<String> = opts.get("session")? else {
                return Ok(err_pair::<bool>(SESSION_REQUIRED_ERR));
            };
            let session_id: CraftId = match raw_id.parse() {
                Ok(id) => id,
                Err(error) => return Ok(err_pair::<bool>(error)),
            };
            let wake: bool = opts.get("wake").unwrap_or(false);
            match SessionMailbox::notify(session_id, text, wake) {
                Ok(()) => Ok((Some(true), None)),
                Err(error) => Ok(err_pair::<bool>(error)),
            }
        })?,
    )?;

    t.set(
        "set_title",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, opts: Table| {
                let tx = tx.clone();
                async move {
                    let req = SessionRequest::SetTitle {
                        id: opts.get("id")?,
                        title: opts.get("title")?,
                    };
                    roundtrip(lua, tx, req).await
                }
            }
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::NO_UI_ERR;
    use mlua::Value;
    use serde_json::json;

    fn lua_with_session(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_session_table(&lua, tx).unwrap();
        lua.globals().set("session", t).unwrap();
        lua
    }

    #[tokio::test]
    async fn live_without_ui_returns_error_pair() {
        let lua = lua_with_session(None);
        let (val, err): (Value, Option<String>) = lua
            .load("return session.live()")
            .eval_async()
            .await
            .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn focus_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Focus { id },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) = lua
            .load("return session.focus('abc')")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), "abc");
    }

    #[tokio::test]
    async fn prompt_forwards_text_and_explicit_session_id() {
        prompt_forwards_text_and_session_id(
            "return session.prompt('hi', { session = 'abc' })",
            Some("abc"),
        )
        .await;
    }

    #[tokio::test]
    async fn prompt_defaults_to_focused_session() {
        prompt_forwards_text_and_session_id("return session.prompt('hi')", None).await;
    }

    async fn prompt_forwards_text_and_session_id(code: &str, expected_id: Option<&str>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Prompt { id, text },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected prompt request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(text, "hi");
            reply_tx.send(Ok(json!("queued"))).unwrap();
        });
        let (val, err): (String, Option<String>) = lua.load(code).eval_async().await.unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(val, "queued");
    }

    #[tokio::test]
    async fn notify_is_synchronous_and_queues_an_observation() {
        let id = CraftId::generate();
        let mailbox = SessionMailbox::register(id);
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval_async()
            .await
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_observation());
        assert_eq!(messages[0].user_text(), Some("built"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn waking_notify_sets_the_mailbox_wake_flag() {
        let id = CraftId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('failed', { session = session_id, wake = true })")
            .eval_async()
            .await
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[tokio::test]
    async fn notify_rejects_missing_and_non_live_sessions() {
        let lua = lua_with_session(None);
        let (_, missing): (Value, Option<String>) = lua
            .load("return session.notify('built')")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(missing.as_deref(), Some(SESSION_REQUIRED_ERR));

        let id = CraftId::generate();
        lua.globals().set("session_id", id.to_string()).unwrap();
        let (_, not_live): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(not_live, Some(format!("session not live: {id}")));
    }

    #[tokio::test]
    async fn notify_rejects_blank_text_and_invalid_session_ids() {
        let lua = lua_with_session(None);
        let (_, blank): (Value, Option<String>) = lua
            .load("return session.notify(' ', { session = 'invalid' })")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(blank.as_deref(), Some(BLANK_NOTIFY_ERR));

        let (_, invalid): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = 'invalid' })")
            .eval_async()
            .await
            .unwrap();
        assert!(invalid.is_some_and(|error| error.contains("invalid base58")));
    }

    #[tokio::test]
    async fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> = lua
            .load("return session.set_title('oops')")
            .eval_async()
            .await;
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
