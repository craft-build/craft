//! `craft.model`: the model behind the focused session. The event loop owns
//! the model slot and the per-session request options, so every call has to
//! round-trip to it.

use mlua::{Error as LuaError, Lua, Result as LuaResult, Table, Value};

use crate::api::util::command::{ModelRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::Pair;

const SET_ARG_ERR: &str = "expected a model spec string or an options table";

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: ModelRequest,
) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Model {
        req,
        reply_tx,
    })
    .await
}

pub(crate) fn create_model_table(
    lua: &Lua,
    tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "get",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, ()| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, ModelRequest::Get).await }
            }
        })?,
    )?;

    t.set(
        "available",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, ()| {
                let tx = tx.clone();
                async move { roundtrip(lua, tx, ModelRequest::Available).await }
            }
        })?,
    )?;

    t.set(
        "set",
        lua.create_async_function({
            let tx = tx.clone();
            move |lua, opts: Value| {
                let tx = tx.clone();
                async move {
                    let req = match opts {
                        Value::String(spec) => ModelRequest::Set {
                            spec: Some(spec.to_str()?.to_owned()),
                            thinking: None,
                            fast: None,
                        },
                        Value::Table(opts) => ModelRequest::Set {
                            spec: opts.get("spec")?,
                            thinking: opts.get("thinking")?,
                            fast: opts.get("fast")?,
                        },
                        other => {
                            return Err(LuaError::runtime(format!(
                                "{SET_ARG_ERR}, got {}",
                                other.type_name()
                            )));
                        }
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
    use crate::api::util::command::{NO_UI_ERR, UI_DROPPED_ERR, UiReply};
    use mlua::LuaSerdeExt;
    use serde_json::{Value as Json, json};
    use test_case::test_case;

    const SPEC: &str = "anthropic/claude-opus-4-6";
    const THINKING: &str = "high";
    const UI_FAILURE: &str = "Model is not allowed by policy: anthropic/claude-opus-4-6";

    fn lua_with_model(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_model_table(&lua, tx).unwrap();
        lua.globals().set("model", t).unwrap();
        lua
    }

    /// The receiver is dropped up front, so the request cannot even leave.
    fn closed_ui() -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        drop(rx);
        lua_with_model(Some(tx))
    }

    /// Answering `None` drops the reply channel, like a UI that took the
    /// request and then vanished.
    fn stub_ui(answer: fn(ModelRequest) -> Option<UiReply>) -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(UiAction::Model { req, reply_tx }) = rx.recv() {
                if let Some(reply) = answer(req) {
                    let _ = reply_tx.send(reply);
                }
            }
        });
        lua_with_model(Some(tx))
    }

    /// Echoes the request back, so a test can assert on what the UI would have
    /// acted on. Fields left out stay `nil` on the way back.
    fn echo(req: ModelRequest) -> Option<UiReply> {
        Some(Ok(match req {
            ModelRequest::Get => json!({ "spec": SPEC, "thinking": THINKING, "fast": true }),
            ModelRequest::Available => json!([SPEC]),
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => json!({ "spec": spec, "thinking": thinking, "fast": fast }),
        }))
    }

    /// The value comes back as JSON so assertions outlive the Lua state.
    async fn eval(lua: &Lua, script: &str) -> (Json, Option<String>) {
        let (val, err): (Value, Option<String>) = lua.load(script).eval_async().await.unwrap();
        (lua.from_value(val).unwrap(), err)
    }

    /// `set` forwards only the fields it was given, so whatever you leave out
    /// the UI leaves alone. `false` and `""` are values though, not omissions:
    /// `""` is the thinking toggle. The last case is the documented loop, `get`
    /// straight back into `set`, read-only extras and all.
    #[test_case("return model.get()", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "get")]
    #[test_case("return model.available()", json!([SPEC]) ; "available")]
    #[test_case("return model.set('anthropic/claude-opus-4-6')", json!({ "spec": SPEC }) ; "set_bare_spec_string")]
    #[test_case("return model.set({ thinking = 8192, fast = true })", json!({ "thinking": "8192", "fast": true }) ; "set_table_without_spec")]
    #[test_case("return model.set({ thinking = '', fast = false })", json!({ "thinking": "", "fast": false }) ; "set_empty_thinking_and_false_fast")]
    #[test_case("local m = model.get() return model.set(m)", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "set_fed_by_get")]
    #[tokio::test]
    async fn requests_cross_the_channel_and_answer_with_the_new_state(
        script: &str,
        expected: Json,
    ) {
        assert_eq!(eval(&stub_ui(echo), script).await, (expected, None));
    }

    /// Every way of not getting an answer lands in the error slot, instead of
    /// throwing or parking forever.
    #[test_case(lua_with_model(None), NO_UI_ERR ; "no_ui_attached")]
    #[test_case(closed_ui(), NO_UI_ERR ; "event_loop_closed")]
    #[test_case(stub_ui(|_| None), UI_DROPPED_ERR ; "reply_channel_dropped")]
    #[test_case(stub_ui(|_| Some(Err(UI_FAILURE.to_owned()))), UI_FAILURE ; "ui_refused")]
    #[tokio::test]
    async fn unanswered_request_returns_an_error_pair(lua: Lua, expected: &str) {
        assert_eq!(
            eval(&lua, "return model.get()").await,
            (Json::Null, Some(expected.to_owned()))
        );
    }

    /// A non-spec argument is a programmer error, so it throws instead of
    /// answering with a pair.
    #[test_case("return model.set(42)" ; "number")]
    #[test_case("return model.set()" ; "no_argument")]
    #[test_case("return model.set(nil)" ; "explicit_nil")]
    #[tokio::test]
    async fn set_throws_on_a_non_spec_argument(script: &str) {
        let lua = lua_with_model(None);
        let err = lua.load(script).eval_async::<Value>().await.unwrap_err();
        assert!(err.to_string().contains(SET_ARG_ERR));
    }
}
