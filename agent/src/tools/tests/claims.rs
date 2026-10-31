//! Scheduling claims per tool family, and the mirrored conflict rule.
//!
//! animist 0.7.0 keeps `ToolScheduling::conflicts` `pub(crate)`
//! (`src/agent/tool.rs`), so [`conflicts`] mirrors its rule for tests: two
//! resource claims conflict iff either side's writes intersect the other's
//! reads or writes; `Exclusive` conflicts with everything (including an
//! empty claim).

use animist::agent::tool::ToolScheduling;
use serde_json::json;

use super::workspace;
use crate::tools::*;

/// Mirror of animist 0.7.0 `ToolScheduling::conflicts` (pub(crate) there).
pub(crate) fn conflicts(a: &ToolScheduling, b: &ToolScheduling) -> bool {
    match (a, b) {
        (
            ToolScheduling::Resources { reads, writes },
            ToolScheduling::Resources {
                reads: other_reads,
                writes: other_writes,
            },
        ) => {
            writes
                .iter()
                .any(|key| other_reads.contains(key) || other_writes.contains(key))
                || other_writes.iter().any(|key| reads.contains(key))
        }
        _ => true,
    }
}

fn claim<T: animist::agent::tool::ToolImpl>(tool: &T, input: serde_json::Value) -> ToolScheduling {
    tool.scheduling(&input)
}

fn reads_of(claim: &ToolScheduling) -> Vec<String> {
    match claim {
        ToolScheduling::Resources { reads, .. } => reads.clone(),
        ToolScheduling::Exclusive => Vec::new(),
    }
}

fn writes_of(claim: &ToolScheduling) -> Vec<String> {
    match claim {
        ToolScheduling::Resources { writes, .. } => writes.clone(),
        ToolScheduling::Exclusive => Vec::new(),
    }
}

fn normalize(workspace: &Workspace, path: &str) -> String {
    crate::run::dedup::normalize_write_path_with_root(
        Some(&workspace.root().display().to_string()),
        path,
    )
}

/// The claim table: every builtin's claim shape for a representative call.
#[test]
fn claim_table_matches_the_documented_scheme() {
    let (_dir, workspace) = workspace();
    let root = normalize(&workspace, &workspace.root().display().to_string());

    // Readers claim reads of their target.
    for (tool, path) in [
        (
            claim(&Read(workspace.clone()), json!({"path": "src/a.rs"})),
            "src/a.rs",
        ),
        (
            claim(&ViewImage(workspace.clone()), json!({"path": "img/a.png"})),
            "img/a.png",
        ),
    ] {
        assert_eq!(reads_of(&tool), vec![normalize(&workspace, path)]);
        assert!(writes_of(&tool).is_empty());
    }

    // Directory walks claim reads of the directory.
    for (tool, dir) in [
        (
            claim(&List(workspace.clone()), json!({"path": "src"})),
            "src",
        ),
        (
            claim(
                &Grep(workspace.clone()),
                json!({"path": "src", "pattern": "x"}),
            ),
            "src",
        ),
        (
            claim(
                &Glob(workspace.clone()),
                json!({"pattern": "*.rs", "path": "src"}),
            ),
            "src",
        ),
    ] {
        assert_eq!(reads_of(&tool), vec![normalize(&workspace, dir)]);
    }

    // Writers claim the target plus its ancestor directories, clamped at
    // the root: `src/a.rs` also writes-claims `src` and the root. Every
    // write therefore shares the root key, so writes serialize
    // workspace-wide while reads of untouched files still overlap.
    let write = claim(
        &Write(workspace.clone()),
        json!({"path": "src/a.rs", "content": "x"}),
    );
    let mut claimed = writes_of(&write);
    claimed.sort();
    let mut expected = vec![
        normalize(&workspace, "src"),
        normalize(&workspace, "src/a.rs"),
        root.clone(),
    ];
    expected.sort();
    assert_eq!(claimed, expected);
    assert!(reads_of(&write).is_empty());

    // Multi-target writers claim every target plus ancestors.
    let delete = claim(
        &Delete(workspace.clone()),
        json!({"files": ["src/a.rs", "doc/b.md"]}),
    );
    assert!(writes_of(&delete).contains(&normalize(&workspace, "src/a.rs")));
    assert!(writes_of(&delete).contains(&normalize(&workspace, "doc/b.md")));
    assert!(writes_of(&delete).contains(&normalize(&workspace, "doc")));
    assert!(writes_of(&delete).contains(&root));

    // Apply-patch claims every parsed target.
    let patch = claim(
        &ApplyPatch(workspace.clone()),
        json!({"patch_text": "*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: new.rs\n+z"}),
    );
    assert!(writes_of(&patch).contains(&normalize(&workspace, "src/a.rs")));
    assert!(writes_of(&patch).contains(&normalize(&workspace, "new.rs")));

    // Exclusive by policy.
    for tool in [
        claim(&Bash::new(workspace.clone()), json!({"command": "ls"})),
        claim(&BashStatus(workspace.clone()), json!({"task_id": "bg_1"})),
        claim(&BashWatch(workspace.clone()), json!({"task_id": "bg_1"})),
        claim(&BashKill(workspace.clone()), json!({"task_id": "bg_1"})),
        claim(
            &Question(std::sync::Arc::new(DismissAsk)),
            json!({"questions": []}),
        ),
        claim(
            &Task(std::sync::Arc::new(NoSubagents)),
            json!({"description": "d", "prompt": "p"}),
        ),
        claim(
            &Review(std::sync::Arc::new(NoSubagents)),
            json!({"task": "t"}),
        ),
        claim(&Batch, json!({"tool_calls": []})),
        claim(
            &MoveFile(workspace.clone()),
            json!({"source": "a.rs", "destination": "b.rs"}),
        ),
    ] {
        assert!(matches!(tool, ToolScheduling::Exclusive), "{tool:?}");
    }

    // Session-state and network tools.
    let todos = claim(&TodoWrite(workspace.clone()), json!({"todos": []}));
    assert_eq!(writes_of(&todos), vec!["todos".to_owned()]);
    for tool in [
        claim(&Webfetch, json!({"url": "https://example.com"})),
        claim(&Websearch, json!({"query": "q"})),
        claim(
            &Retrieve(crate::compression::store::shared_store()),
            json!({"hash": "h"}),
        ),
        claim(
            &Skill::new(workspace.root().to_path_buf()),
            json!({"name": "n"}),
        ),
        claim(&Sessions::new(".".to_owned(), None), json!({})),
        claim(&ListTools(std::sync::Arc::new(Vec::new())), json!({})),
    ] {
        assert!(
            matches!(&tool, ToolScheduling::Resources { reads, writes } if reads.is_empty() && writes.is_empty()),
            "independent claim expected, got {tool:?}"
        );
    }

    // Writers whose arguments name no target stay exclusive (fail closed).
    let empty_delete = claim(&Delete(workspace.clone()), json!({"files": []}));
    assert!(matches!(empty_delete, ToolScheduling::Exclusive));
}

/// The mirrored conflict rule behaves like animist's: read/read overlaps,
/// any write against a matching read or write conflicts, exclusive
/// conflicts with everything, empty claims are independent.
#[test]
fn mirrored_conflict_rule_matches_animist_shape() {
    let resources = |reads: &[&str], writes: &[&str]| ToolScheduling::Resources {
        reads: reads.iter().map(|key| (*key).to_owned()).collect(),
        writes: writes.iter().map(|key| (*key).to_owned()).collect(),
    };
    let read = resources(&["a"], &[]);
    let write = resources(&[], &["a"]);
    let unrelated = resources(&[], &["b"]);
    let independent = resources(&[], &[]);

    assert!(!conflicts(&read, &read), "two reads overlap");
    assert!(conflicts(&read, &write), "read vs write of one path");
    assert!(conflicts(&write, &read), "symmetric");
    assert!(conflicts(&write, &write), "two writes of one path");
    assert!(!conflicts(&write, &unrelated));
    assert!(!conflicts(&write, &independent));
    assert!(conflicts(&ToolScheduling::Exclusive, &independent));
    assert!(conflicts(&independent, &ToolScheduling::Exclusive));
}

/// The claim scheme orders the doc's cases: overlapping reads, serialized
/// same-path write/read, directory walk vs write beneath, conservative
/// same-directory writes.
#[test]
fn claim_scheme_conflicts_match_the_documented_cases() {
    let (_dir, workspace) = workspace();
    let read = claim(&Read(workspace.clone()), json!({"path": "src/a.rs"}));
    let read_again = claim(&Read(workspace.clone()), json!({"path": "src/a.rs"}));
    let read_other = claim(&Read(workspace.clone()), json!({"path": "src/b.rs"}));
    let write_a = claim(
        &Write(workspace.clone()),
        json!({"path": "src/a.rs", "content": "x"}),
    );
    let write_b = claim(
        &Write(workspace.clone()),
        json!({"path": "src/b.rs", "content": "x"}),
    );
    let write_other_dir = claim(
        &Write(workspace.clone()),
        json!({"path": "doc/c.md", "content": "x"}),
    );
    let list_src = claim(&List(workspace.clone()), json!({"path": "src"}));

    assert!(!conflicts(&read, &read_again), "two reads overlap");
    assert!(
        !conflicts(&read, &read_other),
        "reads of sibling files overlap"
    );
    assert!(conflicts(&read, &write_a), "write vs read of the same path");
    assert!(
        conflicts(&list_src, &write_a),
        "directory walk vs write beneath"
    );
    assert!(
        conflicts(&write_a, &write_b),
        "same-directory writes serialize through the shared ancestor"
    );
    assert!(
        conflicts(&write_a, &write_other_dir),
        "every write claims the root ancestor, so writes serialize \
         workspace-wide (the doc's conservative key-shape option)"
    );
}

/// Cross-agent safety: two writes of one file through two separate
/// harnesses (no shared scheduler) serialize through the path lock, so the
/// file ends with one complete write, never an interleaving.
#[tokio::test]
async fn cross_agent_same_file_writes_serialize_through_the_path_lock() {
    let (_dir, workspace) = workspace();
    let a = workspace.clone();
    let b = workspace.clone();
    let (out_a, out_b) = tokio::join!(
        async {
            let mut harness = animist::agent::testing::ToolHarness::builder()
                .build()
                .unwrap();
            harness
                .call(Write(a), json!({"path": "same.txt", "content": "aaaa"}))
                .await
        },
        async {
            let mut harness = animist::agent::testing::ToolHarness::builder()
                .build()
                .unwrap();
            harness
                .call(Write(b), json!({"path": "same.txt", "content": "bbbb"}))
                .await
        },
    );
    let out_a = out_a.expect("first write outcome");
    let out_b = out_b.expect("second write outcome");
    assert!(!out_a.is_error);
    assert!(!out_b.is_error);
    let contents = std::fs::read_to_string(workspace.root().join("same.txt")).unwrap();
    assert!(
        contents == "aaaa" || contents == "bbbb",
        "interleaved write: {contents:?}"
    );
}

/// Cancellation during a long foreground bash kills the process group and
/// reports the partial output with the cancel note.
#[tokio::test]
async fn cancellation_during_a_long_bash_kills_and_reports() {
    // SAFETY: disable the sandbox like the other bash tests.
    unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
    let (_dir, workspace) = workspace();
    let cancel = animist::agent::Cancellation::default();
    let tool = Bash::new(workspace.clone());
    let call = {
        let cancel = cancel.clone();
        async move {
            let mut harness = animist::agent::testing::ToolHarness::builder()
                .cancellation(cancel)
                .build()
                .unwrap();
            harness
                .call(
                    tool,
                    json!({"command": "echo partial; sleep 30", "timeout": 120}),
                )
                .await
        }
    };
    let call = tokio::spawn(call);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("the cancelled bash call returns")
        .unwrap()
        .expect("bash outcome");
    assert!(output.is_error);
    let text = crate::history::ext::tool_content_text(&output.content);
    assert!(text.contains("partial"), "{text}");
    assert!(text.contains("cancelled"), "{text}");
}

/// Cancellation while waiting on the path lock surfaces as the structured
/// lock-cancelled error rather than parking until the holder finishes.
#[tokio::test]
async fn cancellation_during_a_path_lock_wait_returns_promptly() {
    let (_dir, workspace) = workspace();
    // Hold the write lock for "f" across the call so the tool parks on it.
    let holder_cancel = animist::agent::Cancellation::default();
    let keys = crate::tools::claims::write_keys(workspace.root_key().as_deref(), &["f".to_owned()]);
    let _guard = workspace
        .locks()
        .write(&keys, &holder_cancel)
        .await
        .unwrap();

    let cancel = animist::agent::Cancellation::default();
    let call = tokio::spawn({
        let workspace = workspace.clone();
        let cancel = cancel.clone();
        async move {
            let mut harness = animist::agent::testing::ToolHarness::builder()
                .cancellation(cancel)
                .build()
                .unwrap();
            harness
                .call(Write(workspace), json!({"path": "f", "content": "second"}))
                .await
        }
    });
    // The lock is held for far longer than the tool has waited: cancel the
    // run while the tool parks on the write guard.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("the lock wait is cancelled promptly")
        .unwrap();
    let message = outcome
        .err()
        .expect("lock cancellation is a structured error")
        .to_string();
    assert!(
        message.contains("cancelled while waiting for a file lock"),
        "{message}"
    );
}

/// MCP read-only tools claim independence; unannotated ones stay exclusive.
#[test]
fn mcp_claims_follow_the_read_only_hint() {
    let handle = crate::mcp::test_support::stub_handle(&[]);
    let claim = |read_only: Option<bool>| {
        let tool = crate::tools::McpTool {
            handle: handle.clone(),
            qualified: "srv.tool".to_owned(),
            definition: animist::agent::tool::ToolDefinition::builder()
                .name("srv__tool")
                .description("any")
                .input_schema(serde_json::json!({}))
                .build(),
            // Registration maps the server's annotation onto this flag.
            read_only: read_only == Some(true),
        };
        animist::agent::tool::ToolExecutor::new(tool).scheduling(&serde_json::json!({}))
    };

    assert!(
        matches!(
            claim(Some(true)),
            ToolScheduling::Resources { reads, writes } if reads.is_empty() && writes.is_empty()
        ),
        "a server-asserted read-only tool claims independence"
    );
    for hint in [None, Some(false)] {
        assert!(
            matches!(claim(hint), ToolScheduling::Exclusive),
            "hint {hint:?} must stay exclusive"
        );
    }
}
