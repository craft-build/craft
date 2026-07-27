use mlua::{Function, Lua, MultiValue, Result as LuaResult, Table, Value};

use crate::runtime::{enqueue_async_task, register_cancel_hook};

const AWAIT_MIN_ARGS: usize = 2;

pub(crate) fn create_async_table(lua: &Lua) -> LuaResult<Table> {
    let tbl = lua.create_table()?;

    tbl.set(
        "run",
        lua.create_function(|lua, (work_fn, on_finish): (Function, Option<Function>)| {
            let actual_work = if let Some(cb) = on_finish {
                lua.load(
                    r#"
                        local work, finish = ...
                        return function()
                            local ok, result = pcall(work)
                            if ok then
                                finish(nil, result)
                            else
                                finish(result)
                            end
                        end
                    "#,
                )
                .call::<Function>((work_fn, cb))?
            } else {
                work_fn
            };
            let work_key = lua.create_registry_value(actual_work)?;
            enqueue_async_task(lua, work_key)?;
            Ok(())
        })?,
    )?;

    tbl.set(
        "await",
        lua.create_async_function(|lua, args: MultiValue| async move {
            let mut args_vec: Vec<Value> = args.into_vec();
            if args_vec.len() < AWAIT_MIN_ARGS {
                return Err(mlua::Error::runtime(
                    "craft.async.await requires at least 2 arguments: argc, fun, ...",
                ));
            }
            let argc = match &args_vec[0] {
                Value::Integer(n) if *n >= 1 => *n as usize,
                Value::Integer(_) => {
                    return Err(mlua::Error::runtime("argc must be >= 1"));
                }
                _ => return Err(mlua::Error::runtime("argc must be an integer")),
            };
            args_vec.remove(0);
            let fun = match args_vec.remove(0) {
                Value::Function(f) => f,
                _ => return Err(mlua::Error::runtime("second argument must be a function")),
            };

            let (tx, rx) = flume::bounded(1);

            let callback = lua.create_function(move |_lua, values: MultiValue| {
                tx.send(values).ok();
                Ok(())
            })?;

            let insert_pos = (argc - 1).min(args_vec.len());
            args_vec.insert(insert_pos, Value::Function(callback));

            fun.call::<()>(MultiValue::from_iter(args_vec))?;

            let result = rx
                .recv_async()
                .await
                .map_err(|_| mlua::Error::runtime("async.await: callback was never called"))?;
            Ok(result)
        })?,
    )?;

    tbl.set(
        "join",
        lua.load(
            r#"
            local async_tbl = ...
            return function(max_jobs, funs)
                if #funs == 0 then return end
                max_jobs = math.min(max_jobs, #funs)
                local remaining = {}
                for i = max_jobs + 1, #funs do
                    remaining[#remaining + 1] = funs[i]
                end
                local to_go = #funs
                async_tbl.await(1, function(on_finish)
                    local function run_next()
                        to_go = to_go - 1
                        if to_go == 0 then
                            on_finish()
                        elseif #remaining > 0 then
                            async_tbl.run(table.remove(remaining, 1), run_next)
                        end
                    end
                    for i = 1, max_jobs do
                        async_tbl.run(funs[i], run_next)
                    end
                end)
            end
        "#,
        )
        .call::<Function>(&tbl)?,
    )?;

    tbl.set(
        "wrap",
        lua.load(
            r#"
            local async_tbl = ...
            return function(argc, fun)
                return function(...)
                    return async_tbl.await(argc, fun, ...)
                end
            end
        "#,
        )
        .call::<Function>(&tbl)?,
    )?;

    // Register `fn` to run as soon as the current task is cancelled, without
    // waiting for whatever it is doing to finish. Use it to paint the
    // cancelled state: a handler waiting on children stays parked until they
    // wind down, so anything after the wait is too late to reach the screen.
    // The callback runs outside your coroutine, so it must not yield. It
    // fires at most once, immediately if the task is already cancelled. An
    // error inside it is logged and never reaches your handler, and the
    // other hooks still run.
    tbl.set(
        "on_cancel",
        lua.create_function(|lua, f: Function| register_cancel_hook(lua, f))?,
    )?;

    Ok(tbl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{TaskCell, TaskScope};
    use craft_agent::cancel::{CancelToken, CancelTrigger};
    use mlua::Lua;
    use std::sync::Arc;
    use std::time::Duration;
    use test_case::test_case;

    const ERR_TOO_FEW_ARGS: &str =
        "craft.async.await requires at least 2 arguments: argc, fun, ...";
    const ERR_ARGC_GE_1: &str = "argc must be >= 1";
    const ERR_ARGC_INTEGER: &str = "argc must be an integer";
    const ERR_SECOND_ARG_FN: &str = "second argument must be a function";
    const HOOK_NEVER_FIRED: &str = "cancel hook never fired";
    const HOOK_LATE_MSG: &str = "the hook must fire while the wait is still parked, not after it";

    fn setup() -> (Lua, Table) {
        let lua = Lua::new();
        let tbl = create_async_table(&lua).unwrap();
        lua.globals().set("async_tbl", tbl.clone()).unwrap();
        (lua, tbl)
    }

    fn live_scope(lua: &Lua) -> (CancelTrigger, TaskScope) {
        let (trigger, token) = CancelToken::new();
        (
            trigger,
            TaskScope::new(
                lua,
                TaskCell::new(
                    token,
                    None,
                    None,
                    Arc::new(crate::terminal_backend::LocalTerminal),
                ),
            ),
        )
    }

    fn install_notify(lua: &Lua) -> flume::Receiver<()> {
        let (fired_tx, fired_rx) = flume::bounded(1);
        let notify = lua
            .create_function(move |_, ()| {
                fired_tx.send(()).ok();
                Ok(())
            })
            .unwrap();
        lua.globals().set("notify", notify).unwrap();
        fired_rx
    }

    #[test_case(r#"return async_tbl.await(1)"#, ERR_TOO_FEW_ARGS ; "too_few_args")]
    #[test_case(r#"return async_tbl.await(0, function() end)"#, ERR_ARGC_GE_1 ; "argc_below_one")]
    #[test_case(r#"return async_tbl.await(nil, function() end)"#, ERR_ARGC_INTEGER ; "argc_non_integer")]
    #[test_case(r#"return async_tbl.await(1, 42)"#, ERR_SECOND_ARG_FN ; "second_arg_not_fn")]
    #[tokio::test]
    async fn await_validation(code: &str, expected_err: &str) {
        let (lua, _tbl) = setup();
        let err = lua.load(code).eval_async::<Value>().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(expected_err),
            "expected error containing {expected_err:?}, got: {msg}"
        );
    }

    #[test_case(1, &[], 0 ; "no_extra_args")]
    #[test_case(3, &["a", "b"], 2 ; "with_extra_args")]
    #[tokio::test]
    async fn await_callback_insertion_position(argc: usize, extra: &[&str], expected_pos: usize) {
        let (lua, _tbl) = setup();

        let extra_str = extra
            .iter()
            .map(|s| format!(r#""{s}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let trailing = if extra_str.is_empty() {
            String::new()
        } else {
            format!(", {extra_str}")
        };

        let code = format!(
            r#"
                local pos = -1
                local function target(...)
                    local args = {{...}}
                    for i, v in ipairs(args) do
                        if type(v) == "function" then
                            pos = i - 1
                            v()
                            return
                        end
                    end
                end
                async_tbl.await({argc}, target{trailing})
                return pos
                "#
        );

        let result = lua.load(&code).eval_async::<i64>().await.unwrap();
        assert_eq!(result, expected_pos as i64);
    }

    #[tokio::test]
    async fn await_returns_multivalue_from_callback() {
        let (lua, _tbl) = setup();
        let code = r#"
                local function producer(cb)
                    cb("hello", 42, true)
                end
                return async_tbl.await(1, producer)
            "#;
        let results = lua.load(code).eval_async::<MultiValue>().await.unwrap();
        let vals: Vec<Value> = results.into_vec();
        assert_eq!(vals.len(), 3);
        assert_eq!(vals[0].as_string().unwrap().to_string_lossy(), "hello");
        assert_eq!(vals[1].as_integer().unwrap(), 42);
        assert!(vals[2].as_boolean().unwrap());
    }

    #[tokio::test]
    async fn wrap_creates_callable_wrapper() {
        let (lua, _tbl) = setup();
        let code = r#"
                local function async_add(a, b, cb)
                    cb(a + b)
                end
                local wrapped = async_tbl.wrap(3, async_add)
                return wrapped(10, 32)
            "#;
        let result = lua.load(code).eval_async::<i64>().await.unwrap();
        assert_eq!(result, 42);
    }

    /// The composition `batch` leaned on, and the one thing the runtime's own
    /// hook tests cannot show: the handler is parked deep inside a real
    /// `await` whose callback never fires, so the hook runs on a coroutine
    /// that is suspended. Waiting from there is a plugin bug, and the model
    /// must still be told the wait returned once the parked callback fires.
    #[tokio::test]
    async fn on_cancel_fires_while_await_is_still_parked() {
        let (lua, _tbl) = setup();
        let (trigger, scope) = live_scope(&lua);
        let fired_rx = install_notify(&lua);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled_check = Arc::clone(&cancelled);

        let code = r#"
            await_returned = false
            async_tbl.on_cancel(notify)
            async_tbl.await(1, function(cb) parked_cb = cb end)
            await_returned = true
        "#;
        let mut scope_fut = Box::pin(scope.scope_future(lua.load(code).eval_async::<()>()));

        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                biased;
                _ = scope_fut.as_mut() => {}
                _ = async {
                    trigger.cancel();
                    cancelled_check.store(true, std::sync::atomic::Ordering::SeqCst);
                    fired_rx.recv_async().await.expect(HOOK_NEVER_FIRED);
                    assert!(
                        !lua.globals().get::<bool>("await_returned").unwrap(),
                        "{HOOK_LATE_MSG}"
                    );
                    lua.load(r#"parked_cb("done")"#).exec().unwrap();
                    std::future::pending::<()>().await
                } => unreachable!(),
            }
        })
        .await
        .expect("timed out waiting for the cancelled task to wake");

        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "the await must park so the driver gets to cancel"
        );
        assert!(
            lua.globals().get::<bool>("await_returned").unwrap(),
            "releasing the parked callback must let await return"
        );
    }
}
