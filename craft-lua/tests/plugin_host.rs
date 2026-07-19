use std::collections::HashMap;
use std::sync::Arc;

use craft_agent::tools::{ToolRegistry, ToolSource};
use craft_config::{AlwaysThinking, PluginsConfig, ToolOutputLines};
use craft_lua::{Permission, PluginError, PluginHost, PluginPermissions};

fn fresh_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

async fn exec_tool(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = craft_agent::tools::test_support::stub_ctx(&craft_agent::AgentMode::Build);
    inv.execute(&ctx).await.output.map(|out| match out {
        craft_agent::ToolOutput::Plain(s) => s,
        other => panic!("unexpected output: {other:?}"),
    })
}

const ECHO_PLUGIN: &str = r#"
craft.api.register_tool({
    name = "echo_",
    description = "echo",
    schema = {
        type = "object",
        properties = { msg = { type = "string" } },
        required = { "msg" }
    },
    audiences = { "main" },
    handler = function(input, ctx)
        return input.msg
    end
})
"#;

const MINIMAL_SCHEMA: &str =
    r#"{ type = "object", properties = {}, additionalProperties = false }"#;

const STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { url = { type = "string" } },
    required = { "url" },
}"#;

const INVALID_PERMISSION_SCOPE_ERR: &str = "not in schema properties or not type 'string'";
const BAD_NAME_SRC: &str = r#"name = "bad name!", description = "test""#;
const EMPTY_DESC_SRC: &str = r#"name = "valid_name", description = """#;
const EMPTY_AUD_SRC: &str = r#"name = "no_aud", description = "test", audiences = {}"#;
const SCOPE_MISSING_FIELD_SRC: &str =
    r#"name = "bad_scope", description = "test", permission_scopes = "nonexistent""#;
const SCOPE_NON_STRING_FIELD_SRC: &str =
    r#"name = "bad_scope", description = "test", permission_scopes = "count""#;
const OLD_SCOPE_KEY_SRC: &str =
    r#"name = "old_key", description = "test", permission_scope = "url""#;
const WRONG_TYPE_SCOPES_SRC: &str =
    r#"name = "num_scope", description = "test", permission_scopes = 42"#;
const NON_STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { count = { type = "integer" } },
    required = { "count" },
}"#;
const JOB_BAD_CWD: &str = "~/definitely/not/a/dir";
const JOB_BAD_CWD_ERR_PREFIX: &str = "cwd is not a directory: ";
const NIL_WITHOUT_JOBS_ERR: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const FINISH_CALLED_TWICE_ERR: &str = "ctx:finish() already called";
const DEADLINE_ALREADY_SET_ERR: &str = "ctx:set_deadline() already called";
const DEADLINE_TIMEOUT_MSG: &str = "tool deadline_test timed out after 1s";
const BASH_TIMEOUT_MSG: &str = "tool bash timed out after 1s";
const BASH_TIMEOUT_MARKER: &str = "Timed out after 1s";
const PERMISSION_DENIED_PREFIX: &str = "Permission denied:";

#[test]
fn stdlib_globals_accessible() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    for global in &["os", "debug", "string", "table", "math"] {
        let source =
            format!(r#"if {global} == nil then error("stdlib missing: {global} is nil") end"#);
        host.load_source(&format!("stdlib_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("stdlib check for {global} failed: {e}"));
    }
}

#[test]
fn dangerous_globals_blocked() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    for global in &["io", "package"] {
        let source =
            format!(r#"if {global} ~= nil then error("sandbox leak: {global} is not nil") end"#);
        host.load_source(&format!("sandbox_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("sandbox check for {global} failed: {e}"));
    }
}

#[tokio::test]
async fn register_echo_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();

    let entry = reg.get("echo_").expect("echo_ tool not registered");
    assert_eq!(entry.tool.name(), "echo_");
    assert!(
        matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "echo_plugin"),
    );

    let out = exec_tool(&reg, "echo_", serde_json::json!({"msg": "hello"}))
        .await
        .unwrap();
    assert_eq!(out, "hello");
}

#[test]
fn unload_round_trip() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    host.load_source("unload_test", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.unload("unload_test").unwrap();
    assert!(!reg.has("echo_"));

    host.load_source("unload_test", "").unwrap();
    assert!(!reg.has("echo_"));
}

#[test_case::test_case(BAD_NAME_SRC, MINIMAL_SCHEMA, "invalid name" ; "invalid_tool_name")]
#[test_case::test_case(EMPTY_DESC_SRC, MINIMAL_SCHEMA, "description must be non-empty" ; "empty_description")]
#[test_case::test_case(EMPTY_AUD_SRC, MINIMAL_SCHEMA, "audiences" ; "empty_audiences")]
#[test_case::test_case(SCOPE_MISSING_FIELD_SRC, STRING_FIELD_SCHEMA, INVALID_PERMISSION_SCOPE_ERR ; "permission_scopes_missing_field")]
#[test_case::test_case(SCOPE_NON_STRING_FIELD_SRC, NON_STRING_FIELD_SCHEMA, INVALID_PERMISSION_SCOPE_ERR ; "permission_scopes_non_string_field")]
#[test_case::test_case(OLD_SCOPE_KEY_SRC, MINIMAL_SCHEMA, "'permission_scope' was removed" ; "old_permission_scope_key")]
#[test_case::test_case(WRONG_TYPE_SCOPES_SRC, MINIMAL_SCHEMA, "'permission_scopes' must be a string field name or a function" ; "permission_scopes_wrong_type")]
fn registration_validation_rejects(fields: &str, schema: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            {fields},
            schema = {schema},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("validation_test", &src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test_case::test_case(STRING_FIELD_SCHEMA, "nonexistent" ; "missing_field")]
#[test_case::test_case(NON_STRING_FIELD_SCHEMA, "count" ; "non_string_field")]
fn permission_scopes_invalid_rejected(schema: &str, scope_field: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"craft.api.register_tool({{
            name = "bad_scope",
            description = "test",
            schema = {schema},
            permission_scopes = "{scope_field}",
            handler = function() return "" end
        }})"#,
    );
    let err = host
        .load_source("bad_scope_plugin", &src)
        .expect_err("expected error for invalid permission_scopes");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(
        err.to_string().contains(INVALID_PERMISSION_SCOPE_ERR),
        "got: {err}"
    );
}

#[test]
fn permission_scopes_valid_string_field_accepted() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"craft.api.register_tool({{
            name = "ok_scope",
            description = "test",
            schema = {STRING_FIELD_SCHEMA},
            permission_scopes = "url",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("ok_scope_plugin", &src).unwrap();
    assert!(reg.has("ok_scope"));
}

#[tokio::test]
async fn interrupt_kills_infinite_loop_and_vm_recovers() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"
craft.api.register_tool({{
    name = "infinite_loop_",
    description = "loops forever",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) while true do end end
}})
craft.api.register_tool({{
    name = "noop_after_loop",
    description = "returns ok",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) return "ok" end
}})
"#,
    );
    host.load_source("loop_plugin", &src).unwrap();

    let entry = reg.get("infinite_loop_").expect("loop tool not registered");
    let inv = entry.tool.parse(&serde_json::json!({})).unwrap();
    let mut ctx = craft_agent::tools::test_support::stub_ctx(&craft_agent::AgentMode::Build);
    ctx.deadline = craft_agent::tools::Deadline::after(std::time::Duration::from_secs(5));

    let result = inv.execute(&ctx).await;

    assert!(result.output.is_err(), "expected error from timed-out loop");

    let ok = exec_tool(&reg, "noop_after_loop", serde_json::json!({})).await;
    assert!(ok.is_ok(), "VM poisoned after interrupt: {ok:?}");
}

#[test]
fn reload_same_plugin_replaces_tools() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    host.load_source("p1", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.load_source("p1", ECHO_PLUGIN)
        .expect("reload with same plugin name should succeed");
    assert!(reg.has("echo_"));
}

#[test]
fn failed_load_leaves_no_tools_or_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"
craft.api.register_tool({{
    name = "doomed",
    description = "never registered",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "" end
}})
craft.api.register_command({{
    name = "/doomed",
    handler = function() end,
}})
error("plugin blew up after register")
"#,
    );
    let err = host
        .load_source("broken", &src)
        .expect_err("expected lua error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(!reg.has("doomed"));
    assert_eq!(host.command_reader().load().commands.len(), 0);

    host.load_source("broken", ECHO_PLUGIN)
        .expect("retry with good source should succeed");
    assert!(reg.has("echo_"));
}

#[tokio::test]
async fn is_error_propagated_as_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"craft.api.register_tool({{
            name = "returns_error",
            description = "returns is_error=true",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                return {{ llm_output = "boom", is_error = true }}
            end
        }})"#,
    );
    host.load_source("err_plugin", &src).unwrap();

    let err = exec_tool(&reg, "returns_error", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err, "boom");
}

#[tokio::test]
async fn handler_bad_return_type_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "bad_ret_num",
            description = "bad return",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() return 42 end
        }})"#,
    );
    host.load_source("bad_ret", &src).unwrap();

    let err = exec_tool(&reg, "bad_ret_num", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("must return string"), "got: {err}");
}

#[tokio::test]
async fn handler_nil_without_jobs_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = r#"craft.api.register_tool({
        name = "nil_no_jobs",
        description = "returns nil without starting jobs",
        schema = { type = "object", properties = {} },
        audiences = { "main" },
        handler = function() return nil end
    })"#;
    host.load_source("nil_no_jobs", src).unwrap();
    let err = exec_tool(&reg, "nil_no_jobs", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[tokio::test]
async fn handler_lua_error_surfaces_as_tool_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"craft.api.register_tool({{
            name = "thrower",
            description = "throws on call",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() error("intentional kaboom") end
        }})"#,
    );
    host.load_source("thrower_plugin", &src).unwrap();

    let err = exec_tool(&reg, "thrower", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("intentional kaboom"), "got: {err}");
}

#[test]
fn lua_tool_schema_rejects_bad_input() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = r#"
craft.api.register_tool({
    name = "needs_name",
    description = "requires a name field",
    schema = {
        type = "object",
        properties = { name = { type = "string" } },
        required = { "name" }
    },
    handler = function(input) return input.name end
})
"#;
    host.load_source("schema_test", src).unwrap();

    let entry = reg.get("needs_name").unwrap();
    let err = entry
        .tool
        .parse(&serde_json::json!({"count": 1}))
        .err()
        .expect("missing required field should fail");
    assert!(err.to_string().contains("name"));

    assert!(
        entry
            .tool
            .parse(&serde_json::json!({"name": "alice"}))
            .is_ok()
    );
}

#[test]
fn init_lua_with_require_registers_tools() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(lua_dir.join("tools")).unwrap();

    std::fs::write(
        lua_dir.join("tools/greet.lua"),
        r#"
local M = {}
function M.setup()
    craft.api.register_tool({
        name = "greet",
        description = "says hi",
        schema = { type = "object", properties = {}, additionalProperties = false },
        handler = function() return "hi" end
    })
end
return M
"#,
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local greet = require("tools.greet")
greet.setup()
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_plugin_file(&init_path).unwrap();

    assert!(reg.has("greet"));
    assert_eq!(reg.names().len(), 1);
}

#[test]
fn require_caches_modules() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("counter.lua"), "return { value = 42 }\n").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local a = require("counter")
local b = require("counter")
assert(a == b, "require should return cached module")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_sandbox_escape_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"../../escape\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected sandbox error");
    assert!(matches!(err, PluginError::Lua { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("sandbox") || msg.contains("outside"),
        "got: {msg}"
    );
}

#[test]
fn require_circular_returns_sentinel_and_caches_real_value() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(
        lua_dir.join("a.lua"),
        "local b = require(\"b\")\nreturn { name = \"a\" }\n",
    )
    .unwrap();
    std::fs::write(
        lua_dir.join("b.lua"),
        "local a = require(\"a\")\nassert(a == true, \"circular require should return sentinel\")\nreturn { name = \"b\" }\n",
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
require("a")
local a2 = require("a")
assert(type(a2) == "table", "cached value should be table, got: " .. type(a2))
assert(a2.name == "a", "cached value should have name='a'")
local b2 = require("b")
assert(type(b2) == "table", "cached value should be table, got: " .. type(b2))
assert(b2.name == "b", "cached value should have name='b'")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_nonexistent_module_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"nonexistent\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected error for missing module");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("nonexistent"), "got: {err}");
}

#[test]
fn require_error_cleans_loading_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("bad.lua"), "error('deliberate')").unwrap();
    std::fs::write(lua_dir.join("good.lua"), "return { ok = true }").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local ok, err = pcall(require, "bad")
assert(not ok, "bad module should fail")

-- second require of the same broken module must error again, not return a sentinel
local ok2, err2 = pcall(require, "bad")
assert(not ok2, "broken module should fail on retry too")

-- unrelated modules must still work
local g = require("good")
assert(type(g) == "table", "good module should load, got: " .. type(g))
assert(g.ok == true)
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn multi_tool_plugin_registers_and_unloads_all() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"
craft.api.register_tool({{
    name = "multi_alpha",
    description = "first tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "alpha" end
}})
craft.api.register_tool({{
    name = "multi_beta",
    description = "second tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "beta" end
}})
"#,
    );
    host.load_source("multi", &src).unwrap();

    assert!(reg.has("multi_alpha"));
    assert!(reg.has("multi_beta"));

    host.unload("multi").unwrap();
    assert!(!reg.has("multi_alpha"));
    assert!(!reg.has("multi_beta"));
}

#[test]
fn conflict_from_different_plugin_preserves_original() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();

    let src = format!(
        r#"craft.api.register_tool({{
            name = "evolving",
            description = "version 1",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "v1" end
        }})"#,
    );
    host.load_source("keeper", &src).unwrap();
    assert!(reg.has("evolving"));

    let err = host
        .load_source("intruder", &src)
        .expect_err("expected conflict");
    assert!(matches!(err, PluginError::NameConflict { .. }));

    let entry = reg.get("evolving").unwrap();
    assert!(matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "keeper"),);
}

#[tokio::test]
async fn ctx_finish_called_twice_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "double_finish",
            description = "calls finish twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish("first")
                ctx:finish("second")
            end
        }})"#,
    );
    host.load_source("double_finish", &src).unwrap();
    let err = exec_tool(&reg, "double_finish", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains(FINISH_CALLED_TWICE_ERR), "got: {err}");
}

#[tokio::test]
async fn ctx_finish_with_is_error_propagates() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "finish_err",
            description = "finishes with error",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish({{ llm_output = "async boom", is_error = true }})
            end
        }})"#,
    );
    host.load_source("finish_err", &src).unwrap();
    let err = exec_tool(&reg, "finish_err", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err, "async boom");
}

#[tokio::test]
async fn async_job_on_exit_receives_exit_code() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_exit_code",
            description = "reports exit code",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                craft.fn.jobstart("exit 42", {{
                    on_exit = function(job_id, code)
                        ctx:finish("code=" .. tostring(code))
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_exit_code", &src).unwrap();
    let out = exec_tool(&reg, "job_exit_code", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out, "code=42");
}

#[tokio::test]
async fn jobwait_fires_callbacks_while_waiting() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_stream",
            description = "streams lines during jobwait",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local seen = {{}}
                local exit_code
                local id = craft.fn.jobstart("echo a; echo b; exit 7", {{
                    on_stdout = function(_, line) seen[#seen + 1] = line end,
                    on_exit = function(_, code) exit_code = code end,
                }})
                local res = craft.fn.jobwait(id)
                return table.concat(seen, ",")
                    .. " exit=" .. tostring(exit_code)
                    .. " stdout=" .. (res.stdout:gsub("\n", ","))
            end
        }})"#,
    );
    host.load_source("job_stream", &src).unwrap();
    let out = exec_tool(&reg, "job_stream", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out, "a,b exit=7 stdout=a,b");
}

/// Job callbacks must fire while a detached command handler is parked
/// (the homepage `/standup` example: jobstart, then a parked loop).
#[tokio::test]
async fn job_callbacks_fire_while_command_handler_parked() {
    let host = PluginHost::new(fresh_registry(), None).unwrap();
    host.load_source(
        "p",
        r#"
        craft.api.register_command({
            name = "/stream",
            description = "streams job output while parked",
            handler = function()
                craft.fn.jobstart("echo hi", {
                    on_stdout = function(_, line) craft.ui.flash("job:" .. line) end,
                })
                craft.async.await(1, function(_cb) end)
            end,
        })
        "#,
    )
    .unwrap();
    let rx = host.ui_action_rx().expect("ui action rx available");
    let handle = host.event_handle().expect("event handle available");
    handle.run_command(Arc::from("p"), Arc::from("/stream"), String::new());

    let action = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("job callbacks starved while command handler was parked");
    match action {
        craft_lua::UiAction::Flash(msg) => assert_eq!(msg, "job:hi"),
        _ => panic!("expected Flash"),
    }
}

#[tokio::test]
async fn jobstart_invalid_cwd_errors_with_expanded_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_bad_cwd",
            description = "jobstart with missing tilde cwd",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local _, err = pcall(craft.fn.jobstart, "pwd", {{ cwd = "{JOB_BAD_CWD}" }})
                return tostring(err)
            end
        }})"#,
    );
    host.load_source("job_bad_cwd", &src).unwrap();
    let out = exec_tool(&reg, "job_bad_cwd", serde_json::json!({}))
        .await
        .unwrap();
    let expanded = craft_storage::paths::home()
        .expect("home dir")
        .join(JOB_BAD_CWD.strip_prefix("~/").unwrap());
    let expected = format!("{JOB_BAD_CWD_ERR_PREFIX}{}", expanded.display());
    assert!(out.contains(&expected), "got: {out}");
}

#[tokio::test]
async fn async_job_exits_without_finish_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_no_finish",
            description = "job exits but never calls finish",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                craft.fn.jobstart("echo oops", {{
                    on_exit = function(job_id, code) end
                }})
            end
        }})"#,
    );
    host.load_source("job_no_finish", &src).unwrap();
    let err = exec_tool(&reg, "job_no_finish", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[tokio::test]
async fn async_job_callback_error_surfaces() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_cb_err",
            description = "callback throws",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                craft.fn.jobstart("echo trigger", {{
                    on_exit = function(job_id, code)
                        error("callback exploded")
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_cb_err", &src).unwrap();
    let err = exec_tool(&reg, "job_cb_err", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("callback exploded"), "got: {err}");
}

#[tokio::test]
async fn jobstop_kills_running_job() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_stop",
            description = "starts and immediately stops a job",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local id = craft.fn.jobstart("sleep 60", {{
                    on_exit = function(job_id, code)
                        ctx:finish("killed=" .. tostring(code ~= 0))
                    end
                }})
                craft.fn.jobstop(id)
            end
        }})"#,
    );
    host.load_source("job_stop", &src).unwrap();
    let out = exec_tool(&reg, "job_stop", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out, "killed=true");
}

#[tokio::test]
async fn vm_recovers_after_async_job_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"
craft.api.register_tool({{
    name = "async_first",
    description = "async tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        craft.fn.jobstart("echo hi", {{
            on_exit = function(job_id, code) ctx:finish("ok1") end
        }})
    end
}})
craft.api.register_tool({{
    name = "sync_after",
    description = "sync tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "ok2" end
}})
"#,
    );
    host.load_source("recovery", &src).unwrap();
    let out1 = exec_tool(&reg, "async_first", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out1, "ok1");
    let out2 = exec_tool(&reg, "sync_after", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out2, "ok2");
}

const ALREADY_CALLED_ERR: &str = "already called";
const UNKNOWN_FIELD_ERR: &str = "unknown field";

#[test]
fn setup_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "craft.setup({ agent = { max_output_lines = 3000 } })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    let raw = raw.expect("expected Some(RawConfig)");
    assert_eq!(raw.agent.max_output_lines, Some(3000));
}

#[test_case::test_case(
    r#"craft.setup({ agent = { compaction_buffer = 10000 } })"#,
    craft_config::CompactionBuffer::Tokens(10_000)
    ; "compaction_buffer_tokens"
)]
#[test_case::test_case(
    r#"craft.setup({ agent = { compaction_buffer = "15%" } })"#,
    craft_config::CompactionBuffer::Percent(15)
    ; "compaction_buffer_percent"
)]
fn setup_compaction_buffer(lua_src: &str, expected: craft_config::CompactionBuffer) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(lua_src.to_owned(), "test_init.lua".to_owned(), None)
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.agent.compaction_buffer, Some(expected));
}

#[test_case::test_case(
    "craft.setup({ ui = { splash_animaton = false } })",
    UNKNOWN_FIELD_ERR
    ; "unknown_field"
)]
#[test_case::test_case(
    r#"craft.setup({ agent = { max_output_lines = "not a number" } })"#,
    ""
    ; "wrong_type"
)]
#[test_case::test_case(
    "craft.setup({ agent = { bash_timeout_secs = 120 } })",
    UNKNOWN_FIELD_ERR
    ; "moved_plugin_option"
)]
fn setup_rejects_bad_input(lua_src: &str, expected_substr: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .send_run_init_lua(lua_src.to_owned(), "test_init.lua".to_owned(), None)
        .expect_err("expected error");
    assert!(matches!(err, PluginError::Lua { .. }), "got: {err}");
    if !expected_substr.is_empty() {
        assert!(err.to_string().contains(expected_substr), "got: {err}");
    }
}

#[test]
fn setup_double_call_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .send_run_init_lua(
            "craft.setup({})\ncraft.setup({})".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("expected error for double setup");
    assert!(err.to_string().contains(ALREADY_CALLED_ERR), "got: {err}");
}

#[test]
fn setup_not_called_returns_none() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "-- no setup call".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    assert!(raw.is_none());
}

#[test]
fn setup_plugins_section() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "craft.setup({ plugins = { websearch = { enabled = false }, bash = { enabled = true } } })"
                .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.plugins["websearch"].enabled, Some(false));
    assert_eq!(raw.plugins["bash"].enabled, Some(true));
}

#[test]
fn setup_all_sections_at_once() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            r#"craft.setup({
                always_yolo = true,
                always_fast = true,
                always_thinking = "adaptive",
                ui = { splash_animation = false, mouse_scroll_lines = 5 },
                agent = { max_output_lines = 9000 },
                provider = { default_model = "anthropic/claude-opus-4-6" },
                storage = { max_log_files = 3 },
                plugins = { bash = { enabled = true, timeout_secs = 180 } },
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_yolo, Some(true));
    assert_eq!(raw.always_fast, Some(true));
    assert_eq!(
        raw.always_thinking,
        Some(AlwaysThinking::Mode("adaptive".into()))
    );
    assert_eq!(raw.ui.splash_animation, Some(false));
    assert_eq!(raw.ui.mouse_scroll_lines, Some(5));
    assert_eq!(raw.agent.max_output_lines, Some(9000));
    assert_eq!(
        raw.provider.default_model.as_deref(),
        Some("anthropic/claude-opus-4-6")
    );
    assert_eq!(raw.storage.max_log_files, Some(3));
    assert_eq!(raw.plugins["bash"].enabled, Some(true));
    assert_eq!(
        raw.plugins["bash"].opts["timeout_secs"],
        serde_json::json!(180)
    );
}

#[test]
fn setup_always_thinking_accepts_bool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "craft.setup({ always_thinking = true })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_thinking, Some(AlwaysThinking::Toggle(true)));
}

#[test]
fn setup_always_thinking_accepts_number() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "craft.setup({ always_thinking = 8192 })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_thinking, Some(AlwaysThinking::Budget(8192)));
}

#[test]
fn setup_no_tool_registration_in_init_env() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .send_run_init_lua(
            r#"craft.register_tool({
                name = "sneaky",
                description = "should fail",
                audiences = { "main" },
                handler = function() return "nope" end
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("register_tool should not be available in init.lua env");
    assert!(
        matches!(err, PluginError::Lua { .. }),
        "expected Lua error, got: {err}"
    );
}

#[test]
fn register_command_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source(
        "cmd_plugin",
        r#"
        craft.api.register_command({
            name = "/hello",
            description = "says hello",
            handler = function(args) end,
        })
        "#,
    )
    .unwrap();

    let reader = host.command_reader();
    let snap = reader.load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    assert_eq!(snap.commands[0].description.as_ref(), "says hello");
    assert_eq!(snap.commands[0].plugin.as_ref(), "cmd_plugin");
}

#[test_case::test_case(
    r#"craft.api.register_command({ name = "", handler = function() end })"#,
    "non-empty" ; "empty_name"
)]
#[test_case::test_case(
    r#"craft.api.register_command({ name = "/test", description = "no handler" })"#,
    "handler" ; "missing_handler"
)]
fn register_command_validation_rejects(src: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_source("bad_cmd", src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn reload_replaces_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source(
        "reload_cmd",
        r#"craft.api.register_command({ name = "/v1", handler = function() end })"#,
    )
    .unwrap();

    host.load_source(
        "reload_cmd",
        r#"craft.api.register_command({ name = "/v2", handler = function() end })"#,
    )
    .unwrap();
    let snap = host.command_reader().load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/v2");
}

#[test]
fn unload_clears_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source(
        "cmd_only",
        r#"craft.api.register_command({ name = "/bye", handler = function() end })"#,
    )
    .unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 1);

    host.unload("cmd_only").unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 0);
}

#[tokio::test]
async fn job_callback_finishes_after_handler_returns_nil() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "job_after_return",
            description = "on_exit finishes after handler returns nil",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                craft.fn.jobstart("true", {{
                    on_exit = function(_, code)
                        ctx:finish("exit=" .. tostring(code))
                    end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("job_after_return", &src).unwrap();
    let out = exec_tool(&reg, "job_after_return", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out, "exit=0");
}

#[tokio::test]
async fn ctx_set_deadline_times_out() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "deadline_test",
            description = "uses ctx:set_deadline",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(1)
                craft.fn.jobstart("sleep 30", {{
                    on_exit = function(_, _) ctx:finish("should-not-reach") end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("deadline_test", &src).unwrap();
    let err = exec_tool(&reg, "deadline_test", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err, DEADLINE_TIMEOUT_MSG);
}

#[tokio::test]
async fn ctx_set_deadline_twice_errors() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let src = format!(
        r#"craft.api.register_tool({{
            name = "deadline_twice",
            description = "calls set_deadline twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(5)
                ctx:set_deadline(5)
            end
        }})"#,
    );
    host.load_source("deadline_twice", &src).unwrap();
    let err = exec_tool(&reg, "deadline_twice", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains(DEADLINE_ALREADY_SET_ERR), "got: {err}");
}

#[tokio::test]
async fn bash_timeout_round_trip() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .unwrap();
    host.set_sandbox_config(craft_config::SandboxConfig {
        mode: craft_config::SandboxMode::Off,
        ..Default::default()
    });

    let input = serde_json::json!({"command": "sleep 30", "timeout": 1});
    let err = exec_tool(&reg, "bash", input.clone()).await.unwrap_err();
    assert_eq!(err, BASH_TIMEOUT_MSG);

    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();
    let event_tx = craft_agent::EventSender::new(tx, 0);
    handle.request_restore(
        craft_lua::RestoreItem {
            tool: Arc::from("bash"),
            tool_use_id: "test_id".to_owned(),
            output: BASH_TIMEOUT_MSG.to_owned(),
            input,
            is_error: true,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            expanded: false,
        },
        event_tx,
    );
    let _ = handle.collect_prompt_slots_async().await;
    let snapshots: Vec<craft_agent::Envelope> = rx.drain().collect();
    assert_eq!(
        snapshots.len(),
        2,
        "bash restore emits body + header snapshot"
    );
    let body_snapshot = snapshots
        .iter()
        .filter_map(|env| match &env.event {
            craft_agent::AgentEvent::ToolSnapshot { snapshot, .. } => Some(snapshot),
            _ => None,
        })
        .find(|snapshot| {
            let last = snapshot.lines.last().expect("at least one line");
            let text: String = last.spans.iter().map(|s| s.text.as_str()).collect();
            text.contains(BASH_TIMEOUT_MARKER)
        })
        .expect("body snapshot with timeout marker");
    let last = body_snapshot.lines.last().expect("at least one line");
    let text: String = last.spans.iter().map(|s| s.text.as_str()).collect();
    assert!(
        text.contains(BASH_TIMEOUT_MARKER),
        "restored body missing timeout marker; got: {text:?}"
    );
}

#[tokio::test]
async fn memory_write_restore_rebuilds_body_from_input_content() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .unwrap();

    let summary = "wrote n.md (1 lines)";
    let input = serde_json::json!({"command": "write", "path": "n.md", "content": "gamma"});

    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();
    handle.request_restore(
        craft_lua::RestoreItem {
            tool: Arc::from("memory"),
            tool_use_id: "restore_id".to_owned(),
            output: summary.to_owned(),
            input,
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            expanded: true,
        },
        craft_agent::EventSender::new(tx, 0),
    );
    let _ = handle.collect_prompt_slots_async().await;

    let mut text = String::new();
    for env in rx.drain() {
        if let craft_agent::AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            for line in snapshot.lines.iter() {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
            }
        }
    }

    assert!(
        text.contains("gamma"),
        "restored memory body should show saved content, got: {text}"
    );
    assert!(
        !text.contains(summary),
        "restored memory body should not show the summary, got: {text}"
    );
}

async fn restore_snapshot_text(src: &str, tool: &str, expanded: bool) -> String {
    let host = PluginHost::new(fresh_registry(), None).unwrap();
    host.load_source("restore_plugin", src).unwrap();
    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();

    handle.request_restore(
        craft_lua::RestoreItem {
            tool: Arc::from(tool),
            tool_use_id: "restore_id".to_owned(),
            output: "ok".to_owned(),
            input: serde_json::json!({}),
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            expanded,
        },
        craft_agent::EventSender::new(tx, 0),
    );
    let _ = handle.collect_prompt_slots_async().await;

    let mut text = String::new();
    for env in rx.drain() {
        if let craft_agent::AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            for line in snapshot.lines.iter() {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
            }
        }
    }
    text
}

#[test_case::test_case(false, "restore async line" ; "restore_async_task_runs_inline")]
#[test_case::test_case(true, "click async line" ; "click_replay_async_task_runs_inline")]
#[tokio::test]
async fn restore_snapshot_contains_async_run_content(expanded: bool, expected: &str) {
    let src = format!(
        r#"craft.api.register_tool({{
            name = "async_restore",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "ok" end,
            restore = function(input, output, is_error, rctx)
                local buf = craft.ui.buf()
                buf:line("sync line")
                craft.async.run(function()
                    buf:line("restore async line")
                end)
                buf:on("click", function()
                    craft.async.run(function()
                        buf:line("click async line")
                    end)
                end)
                return buf
            end
        }})"#,
    );
    let text = restore_snapshot_text(&src, "async_restore", expanded).await;
    assert!(text.contains("sync line"), "sync content missing: {text}");
    assert!(
        text.contains(expected),
        "async content missing {expected:?}: {text}"
    );
}

async fn exec_tool_with_perms(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = craft_agent::tools::test_support::stub_ctx(&craft_agent::AgentMode::Build);
    inv.execute(&ctx).await.output.map(|out| match out {
        craft_agent::ToolOutput::Plain(s) => s,
        other => panic!("unexpected output: {other:?}"),
    })
}

fn perm_tool_src(api_call: &str) -> String {
    format!(
        r#"
craft.api.register_tool({{
    name = "perm_test",
    description = "test",
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        {api_call}
        ctx:finish("ok")
    end
}})
"#
    )
}

#[test_case::test_case("craft.fs.read('/etc/hosts')" ; "fs_read")]
#[test_case::test_case("craft.fs.write('/tmp/craft-perm-test', 'x')" ; "fs_write")]
#[test_case::test_case("craft.fn.jobstart('echo hi')" ; "run")]
#[tokio::test]
async fn denied_permission_blocks_api(api_call: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let perms = PluginPermissions::denied();
    host.load_source_with_permissions("denied_plugin", &perm_tool_src(api_call), perms)
        .unwrap();
    let err = exec_tool_with_perms(&reg, "perm_test", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(
        err.contains(PERMISSION_DENIED_PREFIX),
        "expected permission denied, got: {err}"
    );
}

#[tokio::test]
async fn user_plugin_with_fs_read_can_read_but_not_write() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let mut perms = PluginPermissions::denied();
    perms.set(Permission::FsRead, true);
    host.load_source_with_permissions(
        "read_only_plugin",
        &perm_tool_src("craft.fs.read('/etc/hosts')"),
        perms,
    )
    .unwrap();
    let result = exec_tool_with_perms(&reg, "perm_test", serde_json::json!({})).await;
    assert!(
        result.is_ok(),
        "fs.read with FsRead permission should succeed, got: {result:?}"
    );
}

#[test]
fn builtin_plugin_has_all_permissions() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .unwrap();
    assert!(reg.has("bash"));
}

#[tokio::test]
async fn env_permission_guards_uv_and_env() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let perms = PluginPermissions::denied();
    host.load_source_with_permissions(
        "no_env_plugin",
        &perm_tool_src("craft.env.state_dir()"),
        perms,
    )
    .unwrap();
    let err = exec_tool_with_perms(&reg, "perm_test", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(
        err.contains(PERMISSION_DENIED_PREFIX),
        "expected permission denied for env, got: {err}"
    );
}

#[tokio::test]
async fn bash_permission_scopes_parseable_command() {
    bash_permission_scopes_never_falls_back_to_json("git status").await;
}

#[tokio::test]
async fn bash_permission_scopes_unparseable_command() {
    bash_permission_scopes_never_falls_back_to_json("echo 'unterminated").await;
}

async fn bash_permission_scopes_never_falls_back_to_json(command: &str) {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
        .unwrap();

    let input = serde_json::json!({ "command": command });
    let entry = reg.get("bash").expect("bash registered");
    let inv = entry.tool.parse(&input).expect("parse failed");
    let scopes = inv
        .permission_scopes()
        .await
        .expect("permission_scopes returned None (would fall back to raw JSON)");

    assert!(
        !scopes.scopes.iter().any(|s| s.contains("\"command\"")),
        "fell back to raw JSON scope: {:?}",
        scopes.scopes
    );
}

const OPTS_PROBE_PLUGIN: &str = r#"
local opts = craft.api.register_options({
    timeout_secs = { default = 120, min = 5, desc = "Timeout." },
    label = { type = "string", desc = "Label." },
})
craft.api.register_tool({
    name = "opts_probe",
    description = "returns merged opts",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        return (craft.json.encode({
            timeout_secs = opts.timeout_secs,
            label = opts.label,
        }))
    end
})
"#;

const UNKNOWN_OPTION_ERR: &str =
    "unknown option \"typo\" for plugins.opts_plugin (valid options: label, timeout_secs)";
const OPTION_TYPE_ERR: &str =
    "invalid value for plugins.opts_plugin.timeout_secs: expected integer";
const OPTION_MIN_ERR: &str =
    "invalid value for plugins.opts_plugin.timeout_secs: 1 is below minimum (5)";
const OPTION_DESC_ERR: &str = "option \"timeout_secs\": desc is required";
const OPTION_NO_TYPE_ERR: &str = "option \"bare\": type is required when there is no default";
const OPTION_SPEC_KEY_ERR: &str = "option \"timeout_secs\": unknown spec key \"mins\"";
const OPTION_DEFAULT_TYPE_ERR: &str =
    "option \"timeout_secs\": default 120 does not match type string";
const OPTION_DEFAULT_MIN_ERR: &str = "option \"timeout_secs\": default 1 is below min (5)";
const OPTION_MIN_ON_STRING_ERR: &str = "option \"label\": min is not allowed for type string";
const OPTION_RESERVED_ERR: &str = "option \"enabled\": reserved name";
const OPTION_TWICE_ERR: &str = "register_options: called more than once";
const UNDECLARED_OPTS_ERR: &str = "unknown options in plugins.bare_plugin: timeout_secs \
(this plugin declares no options via craft.api.register_options)";

fn json_obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().expect("test opts must be an object").clone()
}

#[tokio::test]
async fn register_options_defaults_when_user_sets_nothing() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source_with_opts(
        "opts_plugin",
        OPTS_PROBE_PLUGIN,
        json_obj(serde_json::json!({})),
    )
    .unwrap();

    let out = exec_tool(&reg, "opts_probe", serde_json::json!({}))
        .await
        .unwrap();
    let snap: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(snap["timeout_secs"], serde_json::json!(120));
    assert!(snap["label"].is_null());
}

#[tokio::test]
async fn register_options_user_value_wins_over_default() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    host.load_source_with_opts(
        "opts_plugin",
        OPTS_PROBE_PLUGIN,
        json_obj(serde_json::json!({ "timeout_secs": 30, "label": "x" })),
    )
    .unwrap();

    let out = exec_tool(&reg, "opts_probe", serde_json::json!({}))
        .await
        .unwrap();
    let snap: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(snap["timeout_secs"], serde_json::json!(30));
    assert_eq!(snap["label"], serde_json::json!("x"));
}

#[test_case::test_case(
    serde_json::json!({ "typo": 1 }),
    UNKNOWN_OPTION_ERR
    ; "unknown_key"
)]
#[test_case::test_case(
    serde_json::json!({ "timeout_secs": "abc" }),
    OPTION_TYPE_ERR
    ; "wrong_type"
)]
#[test_case::test_case(
    serde_json::json!({ "timeout_secs": 1 }),
    OPTION_MIN_ERR
    ; "below_min"
)]
fn register_options_rejects_bad_user_opts(opts: serde_json::Value, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_source_with_opts("opts_plugin", OPTS_PROBE_PLUGIN, json_obj(opts))
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(expected), "got: {err}");
}

#[test_case::test_case(
    r#"craft.api.register_options({ timeout_secs = { default = 120 } })"#,
    OPTION_DESC_ERR
    ; "missing_desc"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ bare = { desc = "no type or default" } })"#,
    OPTION_NO_TYPE_ERR
    ; "missing_type_and_default"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ timeout_secs = { default = 120, mins = 5, desc = "T." } })"#,
    OPTION_SPEC_KEY_ERR
    ; "unknown_spec_key"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ timeout_secs = { type = "string", default = 120, desc = "T." } })"#,
    OPTION_DEFAULT_TYPE_ERR
    ; "default_contradicts_type"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ timeout_secs = { default = 1, min = 5, desc = "T." } })"#,
    OPTION_DEFAULT_MIN_ERR
    ; "default_below_min"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ label = { type = "string", min = 1, desc = "L." } })"#,
    OPTION_MIN_ON_STRING_ERR
    ; "min_on_string"
)]
#[test_case::test_case(
    r#"craft.api.register_options({ enabled = { default = true, desc = "E." } })"#,
    OPTION_RESERVED_ERR
    ; "reserved_enabled"
)]
#[test_case::test_case(
    r#"
    craft.api.register_options({ a = { default = 1, desc = "A." } })
    craft.api.register_options({ b = { default = 2, desc = "B." } })
    "#,
    OPTION_TWICE_ERR
    ; "called_twice"
)]
fn register_options_rejects_bad_spec(src: &str, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_source("opts_plugin", src)
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(expected), "got: {err}");
}

#[test]
fn builtin_opts_flow_from_setup_plugins() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let raw = host
        .send_run_init_lua(
            "craft.setup({ plugins = { grep = { search_result_limit = 42 } } })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    host.load_builtins(&PluginsConfig::from_plugins(raw.plugins))
        .unwrap();

    let options = host.plugin_options().unwrap();
    let grep = options.get("grep").expect("grep options registered");
    let limit = grep
        .iter()
        .find(|o| o.name == "search_result_limit")
        .expect("search_result_limit declared");
    assert!(limit.default.is_some(), "declared default surfaces");
    assert!(limit.min.is_some(), "declared min surfaces");
    assert!(!limit.desc.is_empty(), "declared desc surfaces");
}

#[test]
fn undeclared_opts_fail_the_load() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let err = host
        .load_source_with_opts(
            "bare_plugin",
            "local x = 1",
            json_obj(serde_json::json!({ "timeout_secs": 30 })),
        )
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(UNDECLARED_OPTS_ERR), "got: {err}");
}

#[test]
fn opts_for_unknown_plugin_fail_load_builtins() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let mut config = PluginsConfig::from_plugins(HashMap::new());
    config.opts.insert(
        "bsah".to_owned(),
        json_obj(serde_json::json!({ "timeout_secs": 5 })),
    );
    let err = host
        .load_builtins(&config)
        .expect_err("load_builtins should fail");
    assert!(
        err.to_string()
            .contains("plugins.bsah sets options (timeout_secs)"),
        "got: {err}"
    );
}

#[test]
fn unknown_plugin_name_fails_load_builtins() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg), None).unwrap();
    let mut config = PluginsConfig::from_plugins(HashMap::new());
    config.names.push("gerp".to_string());
    let err = host
        .load_builtins(&config)
        .expect_err("load_builtins should fail");
    assert!(
        err.to_string().contains("no bundled plugin named \"gerp\""),
        "got: {err}"
    );
}
