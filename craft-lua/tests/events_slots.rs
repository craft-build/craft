use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_agent::tools::ToolRegistry;
use craft_lua::{PluginHost, SessionEndReason};

const PROBE_SCHEMA: &str = r#"{ type = "object", properties = {}, additionalProperties = false }"#;
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
const DISPATCH_POLL: Duration = Duration::from_millis(10);

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    (reg, host)
}

fn load(host: &PluginHost, name: &str, source: &str) {
    host.load_source(name, source)
        .unwrap_or_else(|e| panic!("{name} failed:\n{e}"));
}

async fn exec_tool(reg: &ToolRegistry, name: &str) -> String {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry
        .tool
        .parse(&serde_json::json!({}))
        .expect("parse failed");
    let ctx = craft_agent::tools::test_support::stub_ctx(&craft_agent::AgentMode::Build);
    let out = inv
        .execute(&ctx)
        .await
        .output
        .unwrap_or_else(|e| panic!("tool {name} failed: {e}"));
    match out {
        craft_agent::ToolOutput::Plain(s) => s,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn probe_tool(name: &str, body: &str) -> String {
    format!(
        r#"
craft.api.register_tool({{
    name = "{name}",
    description = "probe",
    schema = {PROBE_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        {body}
    end
}})
"#
    )
}

#[tokio::test]
async fn exec_autocmds_pattern_routing_and_ev_shape() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_shape",
        r#"
local got, unfiltered = {}, 0
craft.api.create_autocmd("User", { callback = function() unfiltered = unfiltered + 1 end })
craft.api.create_autocmd("User", { pattern = "deploy", callback = function(ev)
    got[#got + 1] = ev
end })
craft.api.exec_autocmds("User", { pattern = "other", data = { n = 1 } })
craft.api.exec_autocmds("User")
assert(#got == 0, "non-matching or absent pattern must not fire filtered listener")
craft.api.exec_autocmds("User", { pattern = "deploy", data = { n = 2 } })
assert(#got == 1, "matching pattern fires")
assert(unfiltered == 3, "unfiltered listener fires always: " .. unfiltered)
local ev = got[1]
assert(ev.event == "User", "ev.event: " .. tostring(ev.event))
assert(ev.match == "deploy", "ev.match: " .. tostring(ev.match))
assert(type(ev.data) == "table" and ev.data.n == 2, "data nested under ev.data")
assert(type(ev.id) == "number", "ev.id is the autocmd id")
"#,
    );
}

#[tokio::test]
async fn autocmd_error_isolation() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_isolation",
        r#"
local ran = false
craft.api.create_autocmd("Err", { callback = function() error("boom") end })
craft.api.create_autocmd("Err", { callback = function() ran = true end })
craft.api.exec_autocmds("Err")
assert(ran, "second callback runs after first errors")
"#,
    );
}

#[tokio::test]
async fn once_callback_semantics() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_once",
        r#"
local n, filtered = 0, 0
craft.api.create_autocmd("Once", { once = true, callback = function()
    n = n + 1
    craft.api.exec_autocmds("Once")
end })
craft.api.create_autocmd("Once", { once = true, pattern = "p", callback = function()
    filtered = filtered + 1
end })
craft.api.exec_autocmds("Once")
assert(n == 1, "reentrant refire: once callback ran " .. n .. " times")
assert(filtered == 0)
craft.api.exec_autocmds("Once", { pattern = "p" })
craft.api.exec_autocmds("Once", { pattern = "p" })
assert(n == 1, "consumed once entry must stay consumed")
assert(filtered == 1, "non-matching fire must not consume a once entry: " .. filtered)
"#,
    );
}

#[tokio::test]
async fn mutual_recursion_stops_at_depth_guard() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_mutual",
        r#"
local x, y = 0, 0
craft.api.create_autocmd("X", { callback = function() x = x + 1; craft.api.exec_autocmds("Y") end })
craft.api.create_autocmd("Y", { callback = function() y = y + 1; craft.api.exec_autocmds("X") end })
craft.api.exec_autocmds("X")
assert(x > 1, "reentrant cross-event dispatch must nest, got " .. x)
assert(x == y and x < 100, "bounded by depth guard, got x=" .. x .. " y=" .. y)
"#,
    );
}

#[tokio::test]
async fn ev_table_fresh_per_callback() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_fresh",
        r#"
local first_saw, second_saw
craft.api.create_autocmd("Fresh", { callback = function(ev)
    ev.injected = true
    first_saw = ev.injected
end })
craft.api.create_autocmd("Fresh", { callback = function(ev) second_saw = ev.injected end })
craft.api.exec_autocmds("Fresh")
assert(first_saw == true and second_saw == nil, "ev mutation must not leak to next callback")
"#,
    );
}

#[tokio::test]
async fn del_autocmd_stops_delivery() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_del",
        r#"
local n = 0
local id = craft.api.create_autocmd("Del", { callback = function() n = n + 1 end })
craft.api.exec_autocmds("Del")
craft.api.del_autocmd(id)
craft.api.exec_autocmds("Del")
assert(n == 1, "deleted autocmd must not fire")
"#,
    );
}

#[tokio::test]
async fn exec_autocmds_throws_on_bad_arg_types() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_bad_args",
        r#"
assert(not pcall(craft.api.exec_autocmds, 42), "event must be string or string[]")
assert(not pcall(craft.api.exec_autocmds, "E", { pattern = 42 }), "pattern must be a string")
assert(not pcall(craft.api.create_autocmd, "E", { callback = function() end, pattern = 42 }))
"#,
    );
}

#[tokio::test]
async fn cross_plugin_event_delivery() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
craft.api.create_autocmd("User", {{ pattern = "deploy", callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.event, tostring(ev.match), tostring(ev.data and ev.data.msg))
end }})
{}
"#,
        probe_tool("probe_events", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);
    load(
        &host,
        "firer",
        r#"
craft.api.exec_autocmds("User", { pattern = "deploy", data = { msg = "hi" } })
craft.api.exec_autocmds("User", { pattern = "nope", data = { msg = "skipped" } })
"#,
    );
    assert_eq!(exec_tool(&reg, "probe_events").await, "User|deploy|hi");
}

#[tokio::test]
async fn host_fired_event_has_new_ev_shape() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
craft.api.create_autocmd("TurnEnd", {{ callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.event, tostring(ev.match), tostring(ev.data and ev.data.k))
end }})
{}
"#,
        probe_tool("probe_turn_end", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);
    host.event_handle()
        .fire_autocmd("TurnEnd", serde_json::json!({ "k": "v" }));
    assert_eq!(exec_tool(&reg, "probe_turn_end").await, "TurnEnd|nil|v");
}

/// A handler has to tell the teardown paths apart, so every reason arrives
/// naming the session it left behind. Only the paths someone waits on carry a
/// deadline, and by the time `end_sessions_blocking` is back the handler has
/// already had its say.
#[tokio::test]
async fn session_end_carries_session_reason_and_deadline() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
craft.api.create_autocmd("SessionEnd", {{ callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.data.session_id, ev.data.reason,
        type(ev.data.deadline_ms))
end }})
{}
"#,
        probe_tool("probe_session_end", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);

    let mut expected = Vec::new();
    for reason in [
        SessionEndReason::Shutdown,
        SessionEndReason::Replaced,
        SessionEndReason::Completed,
    ] {
        let session = craft_storage::id::CraftId::generate();
        host.event_handle().end_sessions_blocking([session], reason);
        expected.push(format!("{session}|{reason}|number"));
    }
    assert_eq!(
        exec_tool(&reg, "probe_session_end").await,
        expected.join(";"),
        "end_sessions_blocking must only return once the handler ran"
    );

    for reason in [
        SessionEndReason::Reset,
        SessionEndReason::Load,
        SessionEndReason::Delete,
    ] {
        let session = craft_storage::id::CraftId::generate();
        host.event_handle().end_session(session, reason);
        expected.push(format!("{session}|{reason}|nil"));
    }

    // Nobody waits on the queued paths, so the log fills in behind our back.
    let expected = expected.join(";");
    let give_up = Instant::now() + DISPATCH_TIMEOUT;
    loop {
        let seen = exec_tool(&reg, "probe_session_end").await;
        if seen == expected {
            return;
        }
        assert!(
            Instant::now() < give_up,
            "SessionEnd handler saw {seen}, expected {expected}"
        );
        std::thread::sleep(DISPATCH_POLL);
    }
}

#[tokio::test]
async fn unload_clears_autocmds_but_keeps_others() {
    let (reg, host) = host();
    let listener = |tool: &str| {
        format!(
            r#"
local n = 0
craft.api.create_autocmd("Shared", {{ callback = function() n = n + 1 end }})
{}
"#,
            probe_tool(tool, "return tostring(n)")
        )
    };
    load(&host, "keep", &listener("probe_keep"));
    load(&host, "gone", &listener("probe_gone"));
    host.unload("gone").unwrap();
    load(&host, "firer", r#"craft.api.exec_autocmds("Shared")"#);
    assert_eq!(exec_tool(&reg, "probe_keep").await, "1");
}

#[tokio::test]
async fn slot_layering_wraps_and_overrides() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_order",
        r#"
local greet = craft.api.declare_slot("greet", function(name) return "hello " .. name end)
craft.api.set_slot("greet", function(prev, name) return prev(name) .. "!" end)
craft.api.set_slot("greet", function(prev, name) return "<" .. prev(name) .. ">" end)
assert(greet("bob") == "<hello bob!>", greet("bob"))

local ov = craft.api.declare_slot("ov", function() return "default" end)
craft.api.set_slot("ov", function(prev) return "override" end)
assert(ov() == "override", "layer may replace without calling prev")
"#,
    );
}

#[tokio::test]
async fn slot_error_after_prev_returns_prev_result_exactly_once() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_late_error",
        r#"
local runs = 0
local s = craft.api.declare_slot("eo", function() runs = runs + 1; return "base" end)
craft.api.set_slot("eo", function(prev)
    local r = prev()
    error("late boom")
end)
local r = s()
assert(r == "base", "chain returns prev's stored result: " .. tostring(r))
assert(runs == 1, "downstream ran exactly once: " .. runs)
"#,
    );
}

#[tokio::test]
async fn slot_error_before_prev_passes_through_once() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_early_error",
        r#"
local runs = 0
local s = craft.api.declare_slot("pb", function(x) runs = runs + 1; return x end)
craft.api.set_slot("pb", function(prev, x) error("early boom") end)
assert(s("v") == "v", "pass-through degradation keeps the chain working")
assert(runs == 1, "rest of chain ran exactly once: " .. runs)
"#,
    );
}

/// Slot dispatch is a plain synchronous call, so `craft.fs.read` parks on the
/// C-call boundary long before it reaches the filesystem. From the default
/// that error travels to the caller, from a layer it costs only the layer and
/// the chain still answers.
#[tokio::test]
async fn slot_chain_must_not_suspend() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_suspends",
        r#"
local d = craft.api.declare_slot("sd", function() return craft.fs.read("/nope") end)
local ok, err = pcall(d)
assert(not ok, "a suspending default must fail the call")
assert(tostring(err):find("yield"), tostring(err))

local l = craft.api.declare_slot("sl", function() return "base" end)
craft.api.set_slot("sl", function(prev) return craft.fs.read("/nope") end)
assert(l() == "base", "a suspending layer is skipped, chain keeps working")
"#,
    );
}

#[tokio::test]
async fn slot_prev_called_twice_errors() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_prev_twice",
        r#"
local s = craft.api.declare_slot("tw", function() return 1 end)
craft.api.set_slot("tw", function(prev)
    prev()
    local ok, err = pcall(prev)
    assert(not ok and tostring(err):find("already consumed"), tostring(err))
    return "done"
end)
assert(s() == "done")
"#,
    );
}

#[tokio::test]
async fn slot_stashed_prev_expires_after_chain_returns() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_stashed_prev",
        r#"
local stash
local s = craft.api.declare_slot("st", function() return 1 end)
craft.api.set_slot("st", function(prev)
    stash = prev
    return prev()
end)
assert(s() == 1)
local ok, err = pcall(stash)
assert(not ok and tostring(err):find("expired"), tostring(err))
"#,
    );
}

#[tokio::test]
async fn slot_default_error_propagates_through_layers() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_default_error",
        r#"
local s = craft.api.declare_slot("de", function() error("default boom") end)
craft.api.set_slot("de", function(prev) return prev() end)
local ok, err = pcall(s)
assert(not ok and tostring(err):find("default boom"), tostring(err))

local r = craft.api.declare_slot("rc", function() error("db") end)
craft.api.set_slot("rc", function(prev)
    local ok2 = pcall(prev)
    assert(not ok2)
    return "recovered"
end)
assert(r() == "recovered", "layer may recover from a failed prev")
"#,
    );
}

#[tokio::test]
async fn slot_recursion_bounded_by_depth_guard() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_recursion",
        r#"
local rd
rd = craft.api.declare_slot("recd", function() return rd() end)
local ok, err = pcall(rd)
assert(not ok and tostring(err):find("exceeded max depth"), tostring(err))

local rf
rf = craft.api.declare_slot("recf", function() return "base" end)
craft.api.set_slot("recf", function(prev) return rf() end)
assert(rf() == "base", "recursive filler degrades to pass-through instead of hanging")
"#,
    );
}

#[tokio::test]
async fn slot_orphan_filler_attaches_on_declare() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_orphan",
        r#"
craft.api.set_slot("oa", function(prev, x) return prev(x) .. "+f" end)
local s = craft.api.declare_slot("oa", function(x) return x end)
assert(s("v") == "v+f", s("v"))
"#,
    );
}

#[tokio::test]
async fn slot_redeclare_errors_including_self() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_dup_self",
        r#"
craft.api.declare_slot("dup", function() end)
local ok, err = pcall(craft.api.declare_slot, "dup", function() end)
assert(not ok and tostring(err):find("already declared"), tostring(err))
"#,
    );
    let err = host
        .load_source(
            "slot_dup_other",
            r#"craft.api.declare_slot("dup", function() end)"#,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("already declared by 'slot_dup_self'"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn get_slots_reports_owner_fillers_and_orphans() {
    let (_reg, host) = host();
    load(
        &host,
        "slots_introspect",
        r#"
craft.api.set_slot("orphan_slot", function(prev) return prev() end)
craft.api.declare_slot("gs", function() return 1 end)
craft.api.set_slot("gs", function(prev) return prev() end)
local slots = craft.api.get_slots()
local gs = slots["gs"]
assert(gs.declared == true and gs.owner == "slots_introspect", tostring(gs.owner))
assert(#gs.fillers == 1 and gs.fillers[1] == "slots_introspect")
local orphan = slots["orphan_slot"]
assert(orphan.declared == false and orphan.owner == nil)
assert(orphan.fillers[1] == "slots_introspect")
"#,
    );
}

const SLOT_CALLER: &str = r#"
local stash
craft.api.create_autocmd("SlotShare", { callback = function(ev) stash = ev.data.callable end })
craft.api.register_tool({
    name = "call_slot",
    description = "probe",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function()
        local ok, res = pcall(stash, "world")
        if ok then return "ok:" .. tostring(res) end
        return "err:" .. tostring(res)
    end
})
"#;

const SLOT_OWNER: &str = r#"
local greet = craft.api.declare_slot("greet", function(name) return "hello " .. name end)
craft.api.exec_autocmds("SlotShare", { data = { callable = greet } })
"#;

const FILLER_EXCLAIM: &str =
    r#"craft.api.set_slot("greet", function(prev, name) return prev(name) .. "!" end)"#;
const FILLER_WRAP: &str =
    r#"craft.api.set_slot("greet", function(prev, name) return "<" .. prev(name) .. ">" end)"#;

#[tokio::test]
async fn slot_reload_semantics() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", SLOT_OWNER);
    load(&host, "exclaim", FILLER_EXCLAIM);
    load(&host, "wrap", FILLER_WRAP);
    assert_eq!(exec_tool(&reg, "call_slot").await, "ok:<hello world!>");

    host.unload("exclaim").unwrap();
    assert_eq!(
        exec_tool(&reg, "call_slot").await,
        "ok:<hello world>",
        "middle filler removed, chain still works"
    );

    host.unload("owner").unwrap();
    let out = exec_tool(&reg, "call_slot").await;
    assert!(
        out.starts_with("err:") && out.contains("slot 'greet' is not declared"),
        "escaped callable after owner unload: {out}"
    );

    load(&host, "owner", SLOT_OWNER);
    assert_eq!(
        exec_tool(&reg, "call_slot").await,
        "ok:<hello world>",
        "surviving filler re-attaches after owner reload"
    );
}
