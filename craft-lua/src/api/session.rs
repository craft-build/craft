//! `craft.session`: host session primitives. Every call round-trips to the UI
//! event loop, which owns the live session runtimes and the session store;
//! the loop answers `list` from a background task so slow scans never block.

use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::command::{SessionReply, SessionRequest, UiAction};
use crate::api::util::convert::json_to_lua;

const NO_UI_ERR: &str = "no interactive UI attached";

type Pair = (Value, Option<String>);

fn err_pair(err: impl ToString) -> Pair {
    (Value::Nil, Some(err.to_string()))
}

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair> {
    let Some(tx) = tx else {
        return Ok(err_pair(NO_UI_ERR));
    };
    let (reply_tx, reply_rx) = flume::bounded::<SessionReply>(1);
    if tx.try_send(UiAction::Session { req, reply_tx }).is_err() {
        return Ok(err_pair(NO_UI_ERR));
    }
    match reply_rx.recv_async().await {
        Ok(Ok(value)) => Ok((json_to_lua(&lua, &value)?, None)),
        Ok(Err(e)) => Ok(err_pair(e)),
        Err(_) => Ok(err_pair("ui event loop dropped the request")),
    }
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
    async fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> = lua
            .load("return session.set_title('oops')")
            .eval_async()
            .await;
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
