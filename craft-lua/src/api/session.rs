//! `craft.session`: host session primitives. Every call round-trips to the UI
//! event loop, which owns the live session runtimes and the session store;
//! the loop answers `list` from a background task so slow scans never block.

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::util::command::{SessionRequest, UiAction, ui_roundtrip};
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, try_pair};

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair<mlua::Value>> {
    let reply =
        try_pair!(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Session { req, reply_tx }).await);
    let value = try_pair!(reply);
    Ok((Some(json_to_lua(&lua, &value)?), None))
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
    async fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> = lua
            .load("return session.set_title('oops')")
            .eval_async()
            .await;
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
