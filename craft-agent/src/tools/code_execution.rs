//! Python execution bridge for the agent.
//!
//! We run Python in the monty sandbox, and bridge tool calls back to the agent.
//! Async awaits get batched via AsyncResolver so concurrent tool calls actually run in parallel.
//! Stdout streams to the UI every STREAM_FLUSH_INTERVAL so you see output as it happens.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

use craft_interpreter::runner::{self, ToolFn};
use craft_interpreter::{AsyncResolver, PendingCall};
use craft_tool_macro::Tool;
use serde::Deserialize;
use serde_json::Value;

use std::sync::Arc;

use crate::agent::tool_dispatch::Emit;
use crate::task_set::TaskSet;
use crate::{AgentConfig, AgentEvent, ToolInput, ToolOutput};

use super::Deadline;
use crate::tools::ToolAudience;

use super::truncate_output;

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const PREAMBLE: &str = "import re\nimport asyncio\nimport sys\nimport os\nimport json\n";
const CANCELLED_ERR: &str = "cancelled";
const TIME_LIMIT_SUBSTR: &str = "time limit exceeded";
const CUT_CANCELLED_FMT: &str = "[cancelled by user; output above is partial]";
const CUT_NO_OUTPUT_CANCELLED: &str = "[cancelled by user; no output before the cut]";

pub const IMAGE_NOT_VISIBLE_NOTE: &str =
    "image pixels are not visible from here; call the view_image tool directly";

// The parent ToolContext with `deadline` capped to the script timeout. Nested
// tool calls dispatch against clones of it, so the live provider, model
// policy, permissions, and shared stores (snapshots, pending edits) flow
// through. A provider-less stub here panics any provider-backed nested path,
// e.g. auto-review permission checks.
struct InterpreterEnv {
    ctx: super::ToolContext,
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct CodeExecution {
    #[param(
        description = "Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file', offset=1, limit=0)`. Use `await asyncio.gather(...)` for concurrency."
    )]
    code: String,
    #[param(description = "Script execution timeout in seconds (default 30)")]
    timeout: Option<u64>,
}

impl CodeExecution {
    pub const NAME: &str = "code_execution";
    pub const DESCRIPTION: &str = include_str!("code_execution.md");
    pub const EXAMPLES: Option<&str> = Some(
        r##"[{"code": "files = (await glob(pattern='**/*.rs')).strip().split('\\n')\nresults = await asyncio.gather(*[read(path=f, offset=1, limit=0) for f in files if f.strip()])\nfor f, c in zip(files, results):\n    if 'fn main' in c: print(f)"},
            {"code": "result = await grep(pattern='TODO', include='*.rs')\nprint(f\"{len(result.strip().splitlines())} TODOs found\")"},
            {"code": "content = await webfetch(url='https://example.com/docs')\nfor line in content.splitlines():\n    if 'auth' in line.lower(): print(line)"}]"##,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let timeout = Duration::from_secs(
            ctx.deadline.cap_timeout(
                self.timeout
                    .unwrap_or(ctx.config.code_execution_timeout_secs),
            )?,
        );
        let code = self.code.clone();
        let tool_use_id = ctx.tool_use_id.clone();
        let config = ctx.config.clone();
        let deadline = Deadline::after(timeout);
        let limits = runner::limits(timeout, config.interpreter_max_memory_mb * 1024 * 1024);
        let mut script_ctx = ctx.clone();
        script_ctx.deadline = deadline;
        let env = InterpreterEnv { ctx: script_ctx };

        // We race cancel against the blocking thread. If cancel wins, the Python thread
        // keeps running till it finishes. Threads can not be killed safely.
        // The accumulator is shared so a cut short still hands back the lines
        // streamed so far instead of a bare error.
        let partial = Arc::new(std::sync::Mutex::new(String::new()));
        let accumulated = Arc::clone(&partial);
        let timeout_secs = timeout.as_secs();
        let result = ctx
            .cancel
            .race(async {
                tokio::task::spawn_blocking(move || {
                    let tools = build_tool_fns(&env);
                    let resolver = build_async_resolver(&env);
                    let code = format!("{PREAMBLE}{code}");

                    let result = if let Some(ref id) = tool_use_id {
                        let mut last_flush = Instant::now();
                        runner::run_streaming(&code, &tools, Some(&resolver), limits, &mut |line| {
                            let mut acc = accumulated.lock().unwrap_or_else(|e| e.into_inner());
                            acc.push_str(line);
                            if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL {
                                env.ctx.event_tx.try_send(AgentEvent::ToolOutput {
                                    id: id.to_string(),
                                    content: acc.clone(),
                                });
                                last_flush = Instant::now();
                            }
                        })
                    } else {
                        runner::run(&code, &tools, Some(&resolver), limits)
                    }
                    .map_err(|e| e.to_string())?;

                    let mut output = String::new();
                    if !result.stdout.is_empty() {
                        output.push_str(result.stdout.trim_end());
                        output.push('\n');
                    }
                    if let Some(ref val) = result.output {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        let _ = write!(output, "return: {val}");
                    }
                    if output.is_empty() {
                        output.push_str("(no output)");
                    }

                    Ok(ToolOutput::Plain(truncate_output(
                        output,
                        env.ctx.config.max_output_lines,
                        env.ctx.config.max_output_bytes,
                    )))
                })
                .await
                .map_err(|e| format!("spawn_blocking failed: {e}"))?
            })
            .await;

        match result {
            Ok(out) => out,
            // Esc or deadline: hand back the lines streamed so far, so the
            // model keeps what the user just watched instead of a bare error.
            Err(e) if e == CANCELLED_ERR => Err(cut_reply(
                &partial.lock().unwrap_or_else(|e| e.into_inner()),
                &config,
                CUT_CANCELLED_FMT,
                CUT_NO_OUTPUT_CANCELLED,
            )),
            Err(e) if e.contains(TIME_LIMIT_SUBSTR) => Err(cut_reply(
                &partial.lock().unwrap_or_else(|e| e.into_inner()),
                &config,
                &format!("[timed out after {timeout_secs}s; output above is partial]"),
                &format!("[timed out after {timeout_secs}s; no output before the cut]"),
            )),
            Err(e) => Err(e),
        }
    }

    pub fn start_header(&self) -> String {
        let lines = self.code.lines().count();
        format!("{lines} lines")
    }
}

/// The partial output a cut-short run streamed, closed with a marker that
/// tells the model the output is real but unfinished. Never claims output the
/// model cannot see above the marker: it would go looking for it, or invent it.
fn cut_reply(
    partial: &str,
    config: &AgentConfig,
    some_output_marker: &str,
    no_output_marker: &str,
) -> String {
    if partial.trim().is_empty() {
        return no_output_marker.to_owned();
    }
    let out = truncate_output(
        partial.to_owned(),
        config.max_output_lines,
        config.max_output_bytes,
    );
    format!("{out}\n{some_output_marker}")
}

super::impl_tool!(
    CodeExecution,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::RESEARCH_SUB
        | super::ToolAudience::GENERAL_SUB,
    kind = "execute",
    tier = super::ToolTier::Core,
    augment = |desc: &mut String, ctx: &super::DescriptionContext| {
        desc.push_str(&super::build_interpreter_tools_description(ctx.filter));
    },
);

impl super::ToolInvocation for CodeExecution {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(CodeExecution::start_header(
            self,
        )))
    }
    fn start_input(&self) -> Option<ToolInput> {
        Some(ToolInput::Script {
            language: "python".into(),
            code: self.code.clone(),
        })
    }
    fn start_annotation(&self) -> Option<String> {
        Some(super::timeout_annotation(self.timeout.unwrap_or(30)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { CodeExecution::execute(&self, ctx).await.into() })
    }
}

fn build_tool_fns(env: &InterpreterEnv) -> HashMap<String, ToolFn> {
    let mut tools: HashMap<String, ToolFn> = HashMap::new();

    for entry in env.ctx.registry.iter().iter() {
        let tool_name = entry.name();
        if !entry.tool.audience().contains(ToolAudience::INTERPRETER) {
            continue;
        }
        if !super::is_tool_enabled(&env.ctx.config, tool_name) {
            continue;
        }
        let name = tool_name.to_string();
        let ctx = env.ctx.clone();

        tools.insert(
            name.clone(),
            Box::new(
                move |fn_name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>| {
                    ctx.deadline.check()?;

                    let input = build_tool_input(&args, &kwargs)?;

                    let done = tokio::runtime::Handle::current().block_on(
                        crate::agent::tool_dispatch::run(
                            &ctx.registry,
                            ctx.mcp.as_ref(),
                            String::new(),
                            fn_name,
                            &input,
                            &ctx,
                            Emit::Silent,
                        ),
                    );
                    super::interpreter_bridge::flatten(&done).map(Value::String)
                },
            ),
        );
    }

    tools
}

fn build_async_resolver(env: &InterpreterEnv) -> AsyncResolver {
    let parent = env.ctx.clone();

    Box::new(move |pending_calls: Vec<PendingCall>| {
        tokio::runtime::Handle::current().block_on(async {
            let call_ids: Vec<u32> = pending_calls.iter().map(|pc| pc.call_id).collect();
            let mut set = TaskSet::new();
            for pc in pending_calls {
                let ctx = parent.clone();

                set.spawn(async move {
                    if let Err(e) = ctx.deadline.check() {
                        return (pc.call_id, Err(e));
                    }

                    let input = match build_tool_input(&pc.args, &pc.kwargs) {
                        Ok(v) => v,
                        Err(e) => return (pc.call_id, Err(e)),
                    };

                    let done = crate::agent::tool_dispatch::run(
                        &ctx.registry,
                        ctx.mcp.as_ref(),
                        String::new(),
                        &pc.name,
                        &input,
                        &ctx,
                        Emit::Silent,
                    )
                    .await;

                    let result = super::interpreter_bridge::flatten(&done).map(Value::String);
                    (pc.call_id, result)
                });
            }

            let results: Vec<_> = set
                .join_all()
                .await
                .into_iter()
                .zip(&call_ids)
                .map(|(r, &call_id)| {
                    r.unwrap_or_else(|msg| {
                        tracing::error!(error = %msg, "code_execution inner tool panicked");
                        (call_id, Err(format!("tool panicked: {msg}")))
                    })
                })
                .collect();

            Ok(results)
        })
    })
}

fn build_tool_input(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value, String> {
    if let Some(first) = args.first()
        && first.is_object()
    {
        return Ok(first.clone());
    }

    if !kwargs.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in kwargs {
            obj.insert(k.clone(), v.clone());
        }
        return Ok(Value::Object(obj));
    }

    if args.is_empty() {
        return Ok(serde_json::json!({}));
    }

    Err("pass arguments as keyword arguments (e.g. read(path='/file'))".into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use async_trait::async_trait;
    use craft_providers::provider::Provider;
    use craft_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::json;
    use test_case::test_case;

    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    use super::*;

    const ALLOW_DECISION: &str = r#"{"verdict":"allow","risk":"low","rationale":"trusted"}"#;

    struct AllowReviewer;

    #[async_trait]
    impl Provider for AllowReviewer {
        async fn stream_message(
            &self,
            _: &Model,
            _: &[Message],
            _: &str,
            _: &Value,
            _: &flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&craft_storage::id::SessionRef>,
        ) -> Result<StreamResponse, crate::AgentError> {
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: ALLOW_DECISION.into(),
                    }],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
            })
        }

        async fn list_models(&self) -> Result<Vec<String>, crate::AgentError> {
            Ok(Vec::new())
        }
    }

    /// Nested tool calls must dispatch with the live parent provider. The
    /// auto-review permission check is provider-backed, so a provider-less
    /// nested context used to panic (`unimplemented!` in NullProvider) and
    /// now fails closed. With the live provider the reviewer allows the
    /// write and the file lands.
    #[tokio::test]
    async fn nested_tool_dispatches_with_live_provider() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested.txt");
        let path_str = path.to_string_lossy();

        let permissions = Arc::new(crate::permissions::PermissionManager::new(
            craft_config::PermissionsConfig {
                default: craft_config::DefaultEffect::Prompt,
                rules: vec![],
                ..Default::default()
            },
            dir.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.toggle_auto_review();

        let mut ctx =
            crate::tools::test_support::stub_ctx_with_permissions(&AgentMode::Build, permissions);
        ctx.provider = Arc::new(AllowReviewer);

        let ci = CodeExecution {
            code: format!("await write(path='{path_str}', content='nested-write-ok')"),
            timeout: None,
        };
        ci.execute(&ctx).await.unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "nested-write-ok");
    }

    #[tokio::test]
    async fn read_tool_via_interpreter() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "line1\nline2\n").unwrap();
        let path_str = path.to_string_lossy();

        let ctx = stub_ctx(&AgentMode::Build);
        let ci = CodeExecution {
            code: format!(
                "result = await read(path='{path_str}', offset=1, limit=0)\nprint(result)"
            ),
            timeout: None,
        };
        let output = ci.execute(&ctx).await.unwrap().as_text();
        assert!(output.contains("line1"));
    }

    #[test_case(&[], &[("path".into(), json!("/foo"))],  json!({"path": "/foo"}) ; "kwargs")]
    #[test_case(&[json!({"path": "/foo"})], &[],         json!({"path": "/foo"}) ; "dict_passthrough")]
    #[test_case(&[], &[],                                json!({})               ; "no_args")]
    fn build_tool_input_cases(args: &[Value], kwargs: &[(String, Value)], expected: Value) {
        assert_eq!(build_tool_input(args, kwargs).unwrap(), expected);
    }

    #[tokio::test]
    async fn cancel_returns_error() {
        let (trigger, cancel) = crate::cancel::CancelToken::new();
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.cancel = cancel;
        let ci = CodeExecution {
            code: "1 + 1".into(),
            timeout: None,
        };
        trigger.cancel();
        let result = ci.execute(&ctx).await;
        assert!(result.is_err());
    }
}
