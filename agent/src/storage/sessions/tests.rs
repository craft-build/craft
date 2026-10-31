use super::records::Record;
use super::{
    ARCHIVE_DIR, ARCHIVE_KEEP, ARCHIVE_MAX_BYTES, CWD_INDEX_FILE, DEFAULT_TITLE, LOG_BLOATED,
    MAX_APPENDS, MAX_TITLE_LEN, MSG_PREFIXES, SESSION_VERSION, StoredSession, StoredSubagent,
    generate_title, json_path, jsonl_path, latest_model_in, latest_thinking_in, load_cwd_index,
    next_epoch, remove_from_cwd_index, update_cwd_index, write_full_session,
};
use super::{HistorySnapshot, Session, SessionError, SessionLog, StorageError};
use crate::history::ext;
use crate::id::CraftId;
use crate::permissions::{Effect, PermissionRule, ToolKey};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use test_case::test_case;

const LEGACY_HEX_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const SONNET_COST: f64 = 0.42;
const HAIKU_COST: f64 = 0.08;

/// A turn that reports no price must not erase what was already billed.
#[test_case(None, None, None ; "unpriced_stays_unpriced")]
#[test_case(None, Some(SONNET_COST), Some(SONNET_COST) ; "first_price_starts_the_total")]
#[test_case(Some(SONNET_COST), Some(HAIKU_COST), Some(SONNET_COST + HAIKU_COST) ; "priced_turns_accumulate")]
#[test_case(Some(SONNET_COST), None, Some(SONNET_COST) ; "unpriced_turn_keeps_the_total")]
fn add_cost_only_grows_a_total(total: Option<f64>, addend: Option<f64>, expected: Option<f64>) {
    assert_eq!(crate::usage::add_cost_opt(total, addend), expected);
}

fn usage(input: u32, cost: Option<f64>) -> super::StoredTokenUsage {
    super::StoredTokenUsage {
        input,
        cost,
        ..Default::default()
    }
}

fn saved_usage_by_model(dir: &Path, id: CraftId) -> Value {
    let text = fs::read_to_string(jsonl_path(dir, id)).unwrap();
    let meta = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["t"] == "meta")
        .expect("saved session has a meta record");
    meta["usage_by_model"].clone()
}

/// What a turn billed is written verbatim, read back verbatim, and keeps
/// adding up after a reload. A later unpriced turn must not throw away what
/// the earlier ones paid.
#[test]
fn recorded_costs_survive_a_reload_and_keep_adding_up() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("anthropic/claude-sonnet-4", "/project");
    session.add_model_usage("claude-sonnet-4", usage(100, Some(SONNET_COST)));
    session.add_model_usage("claude-haiku-4", usage(30, None));
    session.save_to(dir).unwrap();

    let on_disk = saved_usage_by_model(dir, session.id.id());
    assert_eq!(on_disk["claude-sonnet-4"]["cost"], Value::from(SONNET_COST));

    let mut loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    loaded.add_model_usage("claude-sonnet-4", usage(50, None));
    loaded.add_model_usage("claude-haiku-4", usage(10, Some(HAIKU_COST)));

    let sonnet = loaded.usage_by_model()["claude-sonnet-4"];
    assert_eq!((sonnet.input, sonnet.cost), (150, Some(SONNET_COST)));
    let haiku = loaded.usage_by_model()["claude-haiku-4"];
    assert_eq!((haiku.input, haiku.cost), (40, Some(HAIKU_COST)));
}

/// Session files predate `cost`, so an entry without the key loads unpriced
/// with its counters intact, and saving it back must not mint one.
#[test]
fn legacy_usage_entry_loads_unpriced_and_stays_that_way_on_disk() {
    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let json = format!(
        r#"{{"t":"header","v":{SESSION_VERSION},"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/","created_at":0}}
{{"t":"meta","title":"t","token_usage":null,"updated_at":0,"usage_by_model":{{"m":{{"input":7,"output":3}}}}}}"#
    );
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join(format!("{LEGACY_HEX_ID}.jsonl")), json).unwrap();

    let mut loaded = StoredSession::load_from(id, tmp.path()).unwrap();
    let entry = loaded.usage_by_model()["m"];
    assert_eq!(entry.cost, None, "no key means unpriced, not free");
    assert_eq!((entry.input, entry.output), (7, 3));

    let dir = tmp.path().join("rewritten");
    fs::create_dir(&dir).unwrap();
    loaded.save_to(&dir).unwrap();
    assert!(
        saved_usage_by_model(&dir, id)["m"].get("cost").is_none(),
        "an unpriced entry writes no cost key"
    );
}

/// A legacy `.json` session (pre-animist message enums) loads through the
/// converter: messages, tool outputs and subagent histories all arrive in
/// their animist shapes.
#[test]
fn legacy_json_session_loads_converted() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let msg = |text: &str| serde_json::json!({"User": {"content": [{"Text": {"text": text}}]}});
    let json = serde_json::to_string(&serde_json::json!({
        "version": 1,
        "id": LEGACY_HEX_ID,
        "title": "t",
        "cwd": "/p",
        "model": "m",
        "messages": [msg("hi"), {"Assistant": {"content": [{"Text": {"text": "hello"}}]}}],
        "token_usage": {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
        "tool_outputs": {
            "t1": {"call": "t1", "name": "read",
                   "content": [{"Text": {"text": "raw"}}], "is_error": false}
        },
        "subagent_messages": {"call-7": [msg("do things")]},
        "subagents": [],
        "usage_by_model": {},
        "meta": {},
        "created_at": 100,
        "updated_at": 100,
    }))
    .unwrap();
    fs::write(json_path(dir, id), json).unwrap();

    let loaded = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(message_texts(loaded.messages()), ["hi", "hello"]);
    assert!(loaded.tool_outputs().contains_key("t1"));
    assert_eq!(
        message_texts(&loaded.subagent_messages()["call-7"]),
        ["do things"]
    );
    // Saving rewrites it as a canonical current-format log.
    let mut saved = loaded;
    saved.save_to(dir).unwrap();
    assert!(!json_path(dir, id).exists());
    let reloaded = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(
        reloaded.messages(),
        saved.messages(),
        "the rewrite is lossless"
    );
}

// -- message fixtures: source-only entries --

fn user_entry(text: &str) -> animist::history::HistoryEntry {
    ext::entry(ext::stored_user_text(text))
}

fn assistant_entry(text: &str) -> animist::history::HistoryEntry {
    ext::entry(ext::stored(ext::assistant_text(text)))
}

/// Every tool-use id an entry's message carries, for orphan pruning.
fn tool_ids(entry: &animist::history::HistoryEntry) -> Vec<String> {
    ext::tool_calls(entry.verbatim().message())
        .filter_map(|block| match block {
            animist::history::ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn entry_with_call(id: &str) -> animist::history::HistoryEntry {
    let message = animist::history::Message {
        role: animist::history::Role::Assistant,
        content: vec![animist::history::ContentBlock::ToolUse {
            id: id.into(),
            name: "task".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }],
        display_text: None,
        kind: animist::history::MessageKind::Turn,
    };
    ext::entry(ext::stored(message))
}

fn message_texts(entries: &[animist::history::HistoryEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| ext::message_text(entry.verbatim()))
        .collect()
}

fn write_current_jsonl(path: &Path, session: &StoredSession) {
    let mut file = std::fs::File::create(path).unwrap();
    write_full_session(&mut file, session).unwrap();
}

/// Append one entry record after the fact, the way an external writer would.
fn append_raw_entry(path: &Path, entry: animist::history::HistoryEntry) {
    let record = serde_json::to_string(&Record::Entry { entry }).unwrap();
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(record.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
}

#[test]
fn prune_orphans_drops_unreachable_tool_state() {
    fn subagent(id: &str) -> StoredSubagent {
        StoredSubagent {
            tool_use_id: id.into(),
            name: "sub".into(),
            prompt: None,
            model: None,
            lifecycle: None,
            context_mode: None,
        }
    }

    let mut session = StoredSession::new("model", "/p");
    session.push_message(entry_with_call("task-live"));
    session.set_subagent_messages("task-live".into(), vec![entry_with_call("sub-tool")]);
    session.set_subagent_messages("task-stale".into(), vec![entry_with_call("stale-sub-tool")]);
    session.set_subagents(vec![subagent("task-live"), subagent("task-stale")]);
    for id in ["task-live", "sub-tool", "stale-sub-tool", "orphan"] {
        session.insert_tool_output(
            id.into(),
            Arc::new(ext::tool_result(id, Some("read"), "x", false)),
        );
    }

    session.prune_orphans(tool_ids);

    assert_eq!(
        session.subagent_messages().keys().collect::<Vec<_>>(),
        ["task-live"]
    );
    let subagent_ids: Vec<_> = session
        .subagents()
        .iter()
        .map(|sa| sa.tool_use_id.as_str())
        .collect();
    assert_eq!(subagent_ids, ["task-live"]);
    let mut outputs: Vec<_> = session.tool_outputs().keys().cloned().collect();
    outputs.sort();
    assert_eq!(outputs, ["sub-tool", "task-live"]);
}

#[test]
fn roundtrip_save_load() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("anthropic/claude-sonnet-4", "/home/test/project");
    session.push_message(user_entry("hello"));
    session.set_subagent_messages(
        "tool-1".into(),
        vec![user_entry("sub-prompt"), assistant_entry("sub-reply")],
    );
    session.save_to(dir).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
    assert_eq!(loaded.cwd, "/home/test/project");
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(loaded.version, SESSION_VERSION);
    assert_eq!(loaded.subagent_messages()["tool-1"].len(), 2);
    assert_eq!(message_texts(loaded.messages()), ["hello"]);
}

#[test]
fn roundtrip_meta_session_rules_and_context_size() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.meta.session_rules = vec![PermissionRule {
        tool: ToolKey::native("bash"),
        scope: Some("ls".into()),
        effect: Effect::Allow,
    }];
    session.meta.context_size = 120_000;
    session.push_message(user_entry("hi"));
    session.save_to(dir).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.meta.session_rules, session.meta.session_rules);
    assert_eq!(loaded.meta.context_size, 120_000);
}

#[test]
fn replacing_subagent_messages_diverges_the_log() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.set_subagent_messages("sub-1".into(), vec![user_entry("old")]);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.set_subagent_messages("sub-1".into(), vec![user_entry("new")]);
    let err = log.append(&session).unwrap_err();
    assert!(matches!(err, SessionError::LogDiverged { .. }));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.push_message(user_entry("after"));
    log.append(&session).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.subagent_messages()["sub-1"].len(), 1);
    assert_eq!(message_texts(&loaded.subagent_messages()["sub-1"]), ["new"]);
}

#[test]
fn roundtrip_jsonl_incremental() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.push_message(assistant_entry("reply"));
    session.push_message(user_entry("second"));
    // `tool_outputs` is not written; inserting one must not affect the
    // persisted log.
    session.insert_tool_output(
        "tool-1".into(),
        Arc::new(ext::tool_result("tool-1", Some("read"), "ok", false)),
    );
    session.set_subagent_messages("sub-1".into(), vec![user_entry("sub-prompt")]);
    log.append(&session).unwrap();

    let mut sub1 = session.subagent_messages().get("sub-1").unwrap().to_vec();
    sub1.push(assistant_entry("sub-reply"));
    session.set_subagent_messages("sub-1".into(), sub1);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.set_subagent_messages("sub-2".into(), vec![user_entry("sub-2-prompt")]);
    log.append(&session).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 3);
    assert!(
        loaded.tool_outputs().is_empty(),
        "tool outputs are no longer persisted"
    );
    assert_eq!(loaded.subagent_messages()["sub-1"].len(), 2);
    assert_eq!(loaded.subagent_messages()["sub-2"].len(), 1);
}

/// A legacy-format log (`msg` records) still gets an archive copy when a
/// rewrite shrinks it: the archive is the only surviving copy of the turns
/// the rewrite drops.
#[test]
fn shrinking_a_legacy_format_log_still_archives() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let msg = |text: &str| {
        r#"{"t":"msg","d":{"User":{"content":[{"Text":{"text":"REPLACE"}}]}}}"#
            .replace("REPLACE", text)
    };
    let legacy = format!(
        r#"{{"t":"header","v":3,"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/p","created_at":100}}
{}
{}
{}
{{"t":"meta","title":"t","token_usage":null,"updated_at":100}}"#,
        msg("one"),
        msg("two"),
        msg("three"),
    );
    fs::write(jsonl_path(dir, id), legacy).unwrap();

    let mut session = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(session.messages().len(), 3, "legacy records load");
    session.truncate_messages(1);
    session.save_to(dir).unwrap();

    assert_eq!(
        archive_paths(dir, id).len(),
        1,
        "the dropped legacy turns must land in an archive"
    );
    let live = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(live.messages().len(), 1);
    assert!(
        fs::read_to_string(jsonl_path(dir, id))
            .unwrap()
            .contains("\"t\":\"entry\""),
        "the rewrite is in the current format"
    );
}

/// Raw tool-output copies are never written, but records already on
/// disk (from older saves, in their animist block shape) must still load.
#[test]
fn out_records_on_disk_stay_readable() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    let out = serde_json::to_string(&Record::Out {
        id: "t1".into(),
        d: ext::tool_result("t1", Some("read"), "raw copy", false),
    })
    .unwrap();
    file.write_all(out.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert!(loaded.tool_outputs().contains_key("t1"));
}

#[test]
fn append_wrong_session_returns_id_mismatch() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let session_a = StoredSession::new("m", "/project");
    let session_b = StoredSession::new("m", "/project");
    let mut log = SessionLog::rewrite(dir, &session_a).unwrap();

    let err = log.append(&session_b).unwrap_err();
    assert!(matches!(err, SessionError::IdMismatch { .. }));
}

/// An external writer grew the file behind the cursor's back: the append
/// must be refused, and the file must stay loadable.
#[test]
fn append_after_external_write_errors_without_corrupting() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    append_raw_entry(&jsonl_path(dir, session.id.id()), user_entry("external"));

    session.push_message(user_entry("second"));
    let err = log.append(&session).unwrap_err();
    assert!(matches!(
        err,
        SessionError::LogDiverged {
            reason: "file changed underneath"
        }
    ));

    // Rewrite still works and produces a clean log.
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    log.append(&session).unwrap();
    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
    assert_eq!(message_texts(loaded.messages())[1], "second");
}

/// Past [`MAX_APPENDS`] the log is bloated with stale meta records and the
/// next save must go through a canonical rewrite, not another append.
#[test]
fn too_many_appends_forces_canonical_rewrite() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("seed"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    for i in 0..MAX_APPENDS {
        session.push_message(user_entry(&format!("m{i}")));
        log.append(&session).unwrap();
    }
    let before = fs::read(jsonl_path(dir, session.id.id()))
        .unwrap()
        .split(|&b| b == b'\n')
        .count();
    session.push_message(user_entry("one too many"));
    let err = log.append(&session).unwrap_err();
    assert!(matches!(
        err,
        SessionError::LogDiverged {
            reason: LOG_BLOATED
        }
    ));

    let _log = SessionLog::rewrite(dir, &session).unwrap();
    let after = fs::read(jsonl_path(dir, session.id.id()))
        .unwrap()
        .split(|&b| b == b'\n')
        .count();
    assert!(
        after < before,
        "canonical rewrite drops the stale meta records ({after} vs {before})"
    );
    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), MAX_APPENDS + 2);
}

#[test]
fn crash_recovery_truncated_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("survives"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"t\":\"entry\",\"entry\":{\"trun")
        .unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);
}

#[test]
fn crash_recovery_skips_corrupt_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"CORRUPT_LINE\n").unwrap();
    drop(file);
    append_raw_entry(&path, user_entry("after"));

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}

#[test]
fn rewind_compact() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    for i in 0..10 {
        session.push_message(user_entry(&format!("msg-{i}")));
    }
    session.set_subagent_messages(
        "sub-1".into(),
        vec![user_entry("sub-prompt"), assistant_entry("sub-reply")],
    );
    let _ = SessionLog::rewrite(dir, &session).unwrap();

    session.truncate_messages(5);
    session.prune_orphans(tool_ids);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.push_message(user_entry("after-compact-1"));
    session.push_message(user_entry("after-compact-2"));
    session.push_message(user_entry("after-compact-3"));
    log.append(&session).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 8);
    assert!(loaded.subagent_messages().is_empty());
}

#[test]
fn migration_json_to_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("legacy"));

    let json_path = json_path(dir, session.id.id());
    fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
    update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);

    let _log = SessionLog::rewrite(dir, &loaded).unwrap();

    assert!(!json_path.exists());
    assert!(jsonl_path(dir, session.id.id()).exists());

    let reloaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(reloaded.messages().len(), 1);
    assert_eq!(reloaded.model, "m");
}

#[test]
fn load_nonexistent_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let id = CraftId::generate();
    let err = StoredSession::load_from(id, tmp.path()).unwrap_err();
    assert!(matches!(
        err,
        SessionError::Storage {
            source: StorageError::NotFound { .. }
        }
    ));
}

#[test_case("550e8400-e29b-41d4-a716-446655440000")]
#[test_case("550e8400e29b41d4a716446655440000")]
fn load_legacy_hex_filename_migrates_to_canonical(legacy: &str) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = legacy.parse().unwrap();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();
    session.push_message(user_entry("legacy"));
    let legacy_path = dir.join(format!("{legacy}.jsonl"));
    write_current_jsonl(&legacy_path, &session);

    let loaded = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(loaded.id.id(), id);
    assert_eq!(loaded.messages().len(), 1);

    assert!(!legacy_path.exists());
    let canonical = jsonl_path(dir, id);
    assert!(canonical.exists());
}

#[test]
fn list_filters_by_cwd() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s1 = StoredSession::new("m", "/project-a");
    let mut s2 = StoredSession::new("m", "/project-b");
    let mut s3 = StoredSession::new("m", "/project-a");
    s1.save_to(dir).unwrap();
    s2.save_to(dir).unwrap();
    s3.save_to(dir).unwrap();

    let list = StoredSession::list_in(Some("/project-a"), dir).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.id != s2.id));
}

fn save_with_time(session: &mut StoredSession, dir: &Path, time: u64) {
    session.updated_at = time;
    SessionLog::rewrite(dir, session).unwrap();
    update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();
}

#[test]
fn latest_returns_most_recent_for_cwd() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s1 = StoredSession::new("m", "/project");
    s1.title = "first".into();
    save_with_time(&mut s1, dir, 1000);

    let mut s2 = StoredSession::new("m", "/other");
    save_with_time(&mut s2, dir, 2000);

    let mut s3 = StoredSession::new("m", "/project");
    s3.title = "latest".into();
    save_with_time(&mut s3, dir, 3000);

    let latest = StoredSession::latest_in("/project", dir).unwrap().unwrap();
    assert_eq!(latest.title, "latest");
}

#[test]
fn latest_model_reads_the_newest_session_header() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut older = StoredSession::new("zai/glm-5.3", "/project");
    save_with_time(&mut older, dir, 1000);
    let mut newer = StoredSession::new("anthropic/claude-sonnet-4", "/project");
    save_with_time(&mut newer, dir, 2000);

    assert_eq!(
        latest_model_in("/project", dir).unwrap().as_deref(),
        Some("anthropic/claude-sonnet-4")
    );
    // No session for this cwd: nothing to restore.
    assert_eq!(latest_model_in("/elsewhere", dir).unwrap(), None);

    // A stale index entry falls back to scanning the headers on disk.
    let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
    fs::write(
        dir.join(CWD_INDEX_FILE),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    assert_eq!(
        latest_model_in("/project", dir).unwrap().as_deref(),
        Some("anthropic/claude-sonnet-4")
    );
}

#[test]
fn latest_thinking_reads_the_newest_session_meta() {
    use crate::thinking::{Effort, ThinkingConfig};
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut older = StoredSession::new("m", "/project");
    older.set_thinking(ThinkingConfig::Effort(Effort::Low));
    save_with_time(&mut older, dir, 1000);
    let mut newer = StoredSession::new("m", "/project");
    newer.set_thinking(ThinkingConfig::Budget(4096));
    save_with_time(&mut newer, dir, 2000);

    assert_eq!(
        latest_thinking_in("/project", dir).unwrap(),
        Some(ThinkingConfig::Budget(4096))
    );
    // No session for this cwd: nothing to restore.
    assert_eq!(latest_thinking_in("/elsewhere", dir).unwrap(), None);

    // The newest session never recorded a preference: nothing to restore.
    let mut plain = StoredSession::new("m", "/other");
    save_with_time(&mut plain, dir, 3000);
    assert_eq!(latest_thinking_in("/other", dir).unwrap(), None);

    // A stale index entry falls back to scanning the headers on disk.
    let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
    fs::write(
        dir.join(CWD_INDEX_FILE),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    assert_eq!(
        latest_thinking_in("/project", dir).unwrap(),
        Some(ThinkingConfig::Budget(4096))
    );
}

#[test]
fn latest_falls_back_when_index_stale() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.save_to(dir).unwrap();

    let index_path = dir.join(CWD_INDEX_FILE);
    let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
    fs::write(&index_path, serde_json::to_vec(&stale).unwrap()).unwrap();

    let latest = StoredSession::latest_in("/project", dir).unwrap().unwrap();
    assert_eq!(latest.id, session.id);
}

#[test_case("short title", "short title" ; "short_passthrough")]
#[test_case("", DEFAULT_TITLE ; "empty_defaults")]
#[test_case(
    "This is a very long title that exceeds the sixty character limit and should be truncated at a word boundary",
    "This is a very long title that exceeds the sixty character…"
    ; "long_truncates_at_word"
)]
#[test_case("one\n\ntwo\t three", "one two three" ; "whitespace_collapses")]
fn title_extraction(input: &str, expected: &str) {
    let messages: Vec<_> = if input.is_empty() {
        vec![]
    } else {
        vec![user_entry(input)]
    };
    assert_eq!(generate_title(&messages), expected);
}

#[test]
fn dirty_persisted_title_normalized_on_list_and_load() {
    const NORMALIZED: &str = "line one line two";
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s = StoredSession::new("m", "/project");
    s.push_message(user_entry("hi"));
    let mut log = SessionLog::rewrite(dir, &s).unwrap();
    s.set_title("line one\n\n\tline two".into());
    log.append(&s).unwrap();

    let list = StoredSession::list_in(Some("/project"), dir).unwrap();
    assert_eq!(list[0].title, NORMALIZED);
    assert_eq!(
        StoredSession::load_from(s.id.id(), dir).unwrap().title,
        NORMALIZED
    );
}

#[test]
fn delete_removes_file_and_cwd_index() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s1 = StoredSession::new("m", "/project");
    s1.save_to(dir).unwrap();
    let mut s2 = StoredSession::new("m", "/other");
    s2.save_to(dir).unwrap();

    StoredSession::delete_from(s1.id.id(), dir).unwrap();
    assert!(!jsonl_path(dir, s1.id.id()).exists());
    let index = load_cwd_index(dir);
    assert!(!index.values().any(|v| *v == s1.id.to_string()));
    assert_eq!(index.get("/other"), Some(&s2.id.to_string()));
}

#[test]
fn delete_nonexistent_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let id = CraftId::generate();
    let err = StoredSession::delete_from(id, tmp.path()).unwrap_err();
    assert!(matches!(
        err,
        SessionError::Storage {
            source: StorageError::NotFound { .. }
        }
    ));
}

#[test_case("550e8400-e29b-41d4-a716-446655440000")]
#[test_case("550e8400e29b41d4a716446655440000")]
fn delete_legacy_hex_filename_removes_file(legacy: &str) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = legacy.parse().unwrap();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();
    session.push_message(user_entry("legacy"));
    let legacy_path = dir.join(format!("{legacy}.jsonl"));
    write_current_jsonl(&legacy_path, &session);

    StoredSession::delete_from(id, dir).unwrap();
    assert!(!legacy_path.exists());
    let canonical = jsonl_path(dir, id);
    assert!(!canonical.exists());
}

#[test]
fn delete_removes_coexisting_json_and_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("hi"));

    let jsonl_file = jsonl_path(dir, session.id.id());
    write_current_jsonl(&jsonl_file, &session);
    let json_file = json_path(dir, session.id.id());
    fs::write(&json_file, serde_json::to_vec(&session).unwrap()).unwrap();

    StoredSession::delete_from(session.id.id(), dir).unwrap();
    assert!(!jsonl_file.exists());
    assert!(!json_file.exists());
}

#[test]
fn load_picks_jsonl_when_legacy_dual_file_exists() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut jsonl_session = StoredSession::new("m", "/project");
    jsonl_session.id = id.into();
    jsonl_session.push_message(user_entry("newer"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_current_jsonl(&legacy_jsonl, &jsonl_session);

    let mut json_session = StoredSession::new("m", "/project");
    json_session.id = id.into();
    json_session.push_message(user_entry("older"));
    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

    let loaded = StoredSession::load_from(id, dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(message_texts(loaded.messages()), ["newer"]);
}

#[test]
fn load_dual_legacy_files_does_not_leave_duplicate_in_list() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut jsonl_session = StoredSession::new("m", "/project");
    jsonl_session.id = id.into();
    jsonl_session.push_message(user_entry("newer"));
    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_current_jsonl(&legacy_jsonl, &jsonl_session);

    let mut json_session = StoredSession::new("m", "/project");
    json_session.id = id.into();
    json_session.push_message(user_entry("older"));
    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

    StoredSession::load_from(id, dir).unwrap();

    assert!(!legacy_json.exists(), "legacy .json sibling left behind");
    let list = StoredSession::list_in(Some("/project"), dir).unwrap();
    assert_eq!(
        list.len(),
        1,
        "session shows up more than once in the picker"
    );
}

#[test]
fn delete_drains_coexisting_legacy_json_and_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();
    session.push_message(user_entry("legacy"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_current_jsonl(&legacy_jsonl, &session);

    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

    StoredSession::delete_from(id, dir).unwrap();
    assert!(!legacy_jsonl.exists());
    assert!(!legacy_json.exists());
}

/// Two of these already break the byte budget.
const FAKE_ARCHIVE_BYTES: u64 = ARCHIVE_MAX_BYTES / 2;
const EXISTING_ARCHIVE_SEQ: u64 = 7;

fn archive_dir_for(dir: &Path, id: CraftId) -> PathBuf {
    dir.join(ARCHIVE_DIR).join(id.to_string())
}

fn archive_paths(dir: &Path, id: CraftId) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(archive_dir_for(dir, id))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn msg_line_count(path: &Path) -> usize {
    fs::read(path)
        .unwrap()
        .split(|&b| b == b'\n')
        .filter(|line| MSG_PREFIXES.iter().any(|p| line.starts_with(p)))
        .count()
}

#[test]
fn rewrite_dropping_messages_archives_the_old_file() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    for i in 0..5 {
        session.push_message(user_entry(&format!("turn {i}")));
    }
    session.save_to(dir).unwrap();

    session.replace_messages(vec![user_entry("summary")]);
    session.save_to(dir).unwrap();

    let live = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(live.messages().len(), 1);
    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), 1);
    assert_eq!(msg_line_count(&archives[0]), 5);
}

#[test]
fn rewrite_without_shrink_does_not_archive() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    session.push_message(user_entry("one"));
    session.save_to(dir).unwrap();

    session.push_message(user_entry("two"));
    session.save_to(dir).unwrap();

    assert!(!archive_dir_for(dir, session.id.id()).exists());
}

#[test]
fn archived_file_round_trips() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    let pre: Vec<String> = (0..3).map(|i| format!("turn {i}")).collect();
    for text in &pre {
        session.push_message(user_entry(text));
    }
    session.save_to(dir).unwrap();

    session.replace_messages(vec![assistant_entry("summary")]);
    session.save_to(dir).unwrap();

    let scratch = TempDir::new().unwrap();
    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), 1);
    fs::copy(
        &archives[0],
        scratch.path().join(format!("{}.jsonl", session.id.id())),
    )
    .unwrap();
    let archived = StoredSession::load_from(session.id.id(), scratch.path()).unwrap();
    assert_eq!(message_texts(archived.messages()), pre.as_slice());
}

#[test]
fn archive_retention_keeps_newest_three() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    session.push_message(user_entry("seed"));
    session.save_to(dir).unwrap();
    for round in 1..=5 {
        for _ in 0..round {
            session.push_message(user_entry(&format!("turn {round}")));
        }
        session.save_to(dir).unwrap();
        session.replace_messages(vec![user_entry(&format!("summary {round}"))]);
        session.save_to(dir).unwrap();
    }

    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), ARCHIVE_KEEP);
    let mut msg_counts: Vec<usize> = archives.iter().map(|p| msg_line_count(p)).collect();
    msg_counts.sort_unstable();
    assert_eq!(msg_counts, [4, 5, 6]);
}

/// A new name has to beat every name already there, or pruning would read
/// the fresh archive as the oldest and eat it.
#[test]
fn archive_names_count_up_from_the_newest() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    session.push_message(user_entry("one"));
    session.push_message(user_entry("two"));
    session.save_to(dir).unwrap();

    let archive_dir = archive_dir_for(dir, session.id.id());
    fs::create_dir_all(&archive_dir).unwrap();
    let existing = archive_dir.join(format!("{EXISTING_ARCHIVE_SEQ}.jsonl"));
    fs::write(&existing, "").unwrap();

    session.replace_messages(vec![user_entry("summary")]);
    session.save_to(dir).unwrap();

    let fresh = archive_dir.join(format!("{}.jsonl", EXISTING_ARCHIVE_SEQ + 1));
    assert_eq!(
        archive_paths(dir, session.id.id()),
        vec![existing, fresh.clone()]
    );
    assert_eq!(msg_line_count(&fresh), 2);
}

#[test]
fn archive_retention_honors_the_byte_budget() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    session.push_message(user_entry("one"));
    session.push_message(user_entry("two"));
    session.save_to(dir).unwrap();

    let archive_dir = archive_dir_for(dir, session.id.id());
    fs::create_dir_all(&archive_dir).unwrap();
    let fakes: Vec<PathBuf> = (1..=3)
        .map(|ms| {
            let path = archive_dir.join(format!("{ms}.jsonl"));
            // Sparse: the length is all the budget looks at.
            fs::File::create(&path)
                .unwrap()
                .set_len(FAKE_ARCHIVE_BYTES)
                .unwrap();
            path
        })
        .collect();

    session.replace_messages(vec![user_entry("summary")]);
    session.save_to(dir).unwrap();

    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), 2);
    assert!(archives.contains(&fakes[2]));
    assert!(!fakes[0].exists());
    assert!(!fakes[1].exists());
}

#[test]
fn delete_removes_archive_dir() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("model", "/p");
    session.push_message(user_entry("one"));
    session.push_message(user_entry("two"));
    session.save_to(dir).unwrap();
    session.replace_messages(vec![user_entry("summary")]);
    session.save_to(dir).unwrap();
    let archive_dir = archive_dir_for(dir, session.id.id());
    assert!(archive_dir.exists());

    StoredSession::delete_from(session.id.id(), dir).unwrap();
    assert!(!archive_dir.exists());
    assert!(!jsonl_path(dir, session.id.id()).exists());
}

#[test]
fn rewrite_removes_legacy_named_files() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();
    session.push_message(user_entry("legacy"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_current_jsonl(&legacy_jsonl, &session);

    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

    let _log = SessionLog::rewrite(dir, &session).unwrap();

    assert!(!legacy_jsonl.exists());
    assert!(!legacy_json.exists());
    assert!(jsonl_path(dir, id).exists());
}

#[test]
fn load_migration_does_not_steal_latest_pointer() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut newest = StoredSession::new("m", "/project");
    newest.title = "newest".into();
    save_with_time(&mut newest, dir, 3000);

    let mut older = StoredSession::new("m", "/project");
    older.title = "older".into();
    older.updated_at = 1000;
    let json_path = json_path(dir, older.id.id());
    fs::write(&json_path, serde_json::to_vec(&older).unwrap()).unwrap();

    // Opening the older session migrates it to canonical jsonl, but must not
    // repoint cwd->latest at it.
    let loaded = StoredSession::load_from(older.id.id(), dir).unwrap();
    assert_eq!(loaded.title, "older");
    assert!(!json_path.exists());

    let latest = StoredSession::latest_in("/project", dir).unwrap().unwrap();
    assert_eq!(latest.title, "newest");
}

#[test]
fn load_surfaces_corrupt_header_id() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let id = CraftId::generate();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();

    let path = jsonl_path(dir, id);
    write_current_jsonl(&path, &session);

    let corrupted =
        fs::read_to_string(&path)
            .unwrap()
            .replacen(&id.to_string(), "not-a-valid-id", 1);
    fs::write(&path, corrupted).unwrap();

    let err = StoredSession::load_from(id, dir).unwrap_err();
    assert!(matches!(err, SessionError::CorruptHeaderId { .. }));
}

#[test]
fn remove_from_cwd_index_matches_legacy_hex_value() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let legacy = "550e8400-e29b-41d4-a716-446655440000";
    let id: CraftId = legacy.parse().unwrap();
    let mut session = StoredSession::new("m", "/project");
    session.id = id.into();

    let mut index: HashMap<String, String> = HashMap::new();
    index.insert("/project".into(), legacy.to_string());
    fs::write(
        dir.join(CWD_INDEX_FILE),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();

    remove_from_cwd_index(dir, session.id.id()).unwrap();
    let after = load_cwd_index(dir);
    assert!(!after.contains_key("/project"));
}

#[test]
fn title_unicode_safe() {
    let input = "あ".repeat(100);
    let title = generate_title(&[user_entry(&input)]);
    assert!(title.len() <= MAX_TITLE_LEN * 4);
    assert!(title.is_char_boundary(title.len()));
}

#[test]
fn scan_headers_reads_both_formats() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut s1 = StoredSession::new("m", "/project");
    s1.title = "jsonl-session".into();
    s1.save_to(dir).unwrap();

    let mut s2 = StoredSession::new("m", "/project");
    s2.title = "json-session".into();
    let json_path = json_path(dir, s2.id.id());
    fs::write(&json_path, serde_json::to_vec(&s2).unwrap()).unwrap();

    let list = StoredSession::list_in(Some("/project"), dir).unwrap();
    assert_eq!(list.len(), 2);
}

#[test]
fn scan_jsonl_reads_trailing_meta_from_tail() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut session = StoredSession::new("m", "/project");
    session.save_to(dir).unwrap();
    let path = jsonl_path(dir, session.id.id());

    let header_line = fs::read_to_string(&path).unwrap();
    let mut buf = header_line;
    for i in 0..5000u64 {
        buf.push_str(&format!("{{\"t\":\"out\",\"id\":\"tool-{i}\"}}\n"));
    }
    buf.push_str("{\"t\":\"meta\",\"title\":\"final-title\",\"updated_at\":99999}\n");
    fs::write(&path, buf).unwrap();

    let list = StoredSession::list_in(Some("/project"), dir).unwrap();
    let summary = list.iter().find(|s| s.id == session.id).unwrap();
    assert_eq!(summary.title, "final-title");
    assert_eq!(summary.updated_at, 99999);
}

#[test]
fn load_wrong_version_legacy_returns_error() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("test/model", "/tmp");
    session.version = 999;
    let path = json_path(dir, session.id.id());
    fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

    let err = StoredSession::load_from(session.id.id(), dir).unwrap_err();
    assert!(matches!(
        err,
        SessionError::VersionMismatch { found: 999, .. }
    ));
}

#[test]
fn open_roundtrip_resumes_append() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.push_message(assistant_entry("reply"));
    log.append(&session).unwrap();
    drop(log);

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);

    session.push_message(user_entry("second"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    log.append(&session).unwrap();
    drop(log);

    let reloaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(reloaded.messages().len(), 3);
}

#[test]
fn load_wrong_version_jsonl_returns_error() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let bad_header = serde_json::json!({
        "t": "header",
        "v": 999,
        "id": "01965087-4c71-7f00-8000-000000000000",
        "model": "m",
        "cwd": "/tmp",
        "created_at": 0
    });
    let id: CraftId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
    let path = jsonl_path(dir, id);
    fs::write(&path, format!("{}\n", bad_header)).unwrap();

    let err = StoredSession::load_from(id, dir).unwrap_err();
    assert!(matches!(
        err,
        SessionError::VersionMismatch { found: 999, .. }
    ));
}

/// The mirror re-adopts the run's snapshot on every checkpoint; that must
/// not erase the void minted by a same-frame in-place replacement, or the
/// writer appends onto a stale prefix and persists a mixed transcript.
#[test]
fn snapshot_adoption_does_not_erase_a_local_rewrite() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: Arc<StoredSession> = Arc::new(StoredSession::new("m", "/project"));
    let run = HistorySnapshot {
        epoch: next_epoch(),
        messages: Arc::new(vec![user_entry("hi")]),
    };
    let meta = session.meta.clone();
    Session::checkpoint(
        &mut session,
        Some(&run),
        meta.clone(),
        super::StoredTokenUsage::default(),
    );
    Arc::make_mut(&mut session).set_subagent_messages("sub-1".into(), vec![user_entry("old")]);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    Arc::make_mut(&mut session).set_subagent_messages("sub-1".into(), vec![user_entry("new")]);
    let advanced = HistorySnapshot {
        epoch: run.epoch,
        messages: Arc::new(vec![user_entry("hi"), assistant_entry("reply")]),
    };
    Session::checkpoint(
        &mut session,
        Some(&advanced),
        meta,
        super::StoredTokenUsage::default(),
    );
    log.append(&session)
        .expect_err("append must be refused after a local rewrite");

    SessionLog::rewrite(dir, &session).unwrap();
    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), session.messages().len());
    assert_eq!(
        loaded.subagent_messages().get("sub-1").map(|m| m.len()),
        Some(1)
    );
    assert_eq!(message_texts(&loaded.subagent_messages()["sub-1"]), ["new"]);
}

#[test]
fn crash_recovery_preserves_tool_outputs_around_corrupt_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    let out = serde_json::to_string(&Record::Out {
        id: "t1".into(),
        d: ext::tool_result("t1", Some("read"), "ok", false),
    })
    .unwrap();
    file.write_all(out.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
    file.write_all(b"CORRUPT\n").unwrap();
    drop(file);
    append_raw_entry(&path, user_entry("second"));

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
    assert!(loaded.tool_outputs().contains_key("t1"));
}

#[test]
fn corrupt_header_line_only_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let id: CraftId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
    let path = jsonl_path(dir, id);
    fs::write(&path, "NOT_A_HEADER\n").unwrap();

    let err = StoredSession::load_from(id, dir).unwrap_err();
    assert!(matches!(
        err,
        SessionError::Storage {
            source: StorageError::NotFound { .. }
        }
    ));
}

#[test]
fn empty_lines_in_jsonl_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("msg"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"\n\n\n").unwrap();
    drop(file);
    append_raw_entry(&path, user_entry("after"));

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}

#[test]
fn unknown_record_type_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.push_message(user_entry("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"t\":\"future_type\",\"d\":{}}\n")
        .unwrap();
    drop(file);
    append_raw_entry(&path, user_entry("second"));

    let loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}

/// Mode persistence round-trip: a session stored with `set_mode` reads its
/// mode back after save/load, in both spellings a writer can produce.
#[test]
fn set_mode_round_trips_through_save_and_load() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session = StoredSession::new("m", "/project");
    session.set_mode(Some("plan".into()));
    session.save_to(dir).unwrap();
    let mut loaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.meta.mode.as_deref(), Some("plan"));

    loaded.set_mode(Some("build".into()));
    loaded.save_to(dir).unwrap();
    let reloaded = StoredSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(reloaded.meta.mode.as_deref(), Some("build"));
}

/// A meta record without `mode` (sessions predating persistence) loads as
/// `None`, so resume falls back to the CLI-seeded mode.
#[test]
fn legacy_meta_without_mode_loads_as_none() {
    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let json = format!(
        r#"{{"t":"header","v":{SESSION_VERSION},"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/","created_at":0}}
{{"t":"meta","title":"t","token_usage":null,"updated_at":0}}"#
    );
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join(format!("{LEGACY_HEX_ID}.jsonl")), json).unwrap();
    let loaded = StoredSession::load_from(id, tmp.path()).unwrap();
    assert_eq!(loaded.meta.mode, None);
}

// -- the 04 format cutover: golden legacy fixtures, conversion, round trips --

mod fixtures {
    use super::*;
    use crate::history::{ContentBlock, Role, ToolContent, marker};

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/storage/sessions/fixtures"
        ))
        .join(format!("{name}.jsonl"))
    }

    /// Fixtures carry their own session ids; read the header id off disk and
    /// load through the ordinary path.
    /// Fixtures live under content-addressed test names, not their internal
    /// session ids, so they load through the file path itself.
    fn load_fixture(name: &str) -> StoredSession {
        super::super::records::load_session_at(&fixture_path(name)).unwrap()
    }

    fn message_at(session: &StoredSession, index: usize) -> &animist::history::HistoryEntry {
        &session.messages()[index]
    }

    #[test]
    fn text_fixture_loads_as_animist_messages() {
        let session = load_fixture("text");
        assert_eq!(message_texts(session.messages()).len(), 3);
        let system = message_at(&session, 0).verbatim().message();
        assert_eq!(system.role, Role::System);
        assert_eq!(ext::text(system), "You are craft.");
        assert_eq!(
            ext::text(message_at(&session, 1).verbatim().message()),
            "hello"
        );
        assert_eq!(
            ext::text(message_at(&session, 2).verbatim().message()),
            "hi there"
        );
    }

    #[test]
    fn reasoning_signatures_survive_the_conversion() {
        let session = load_fixture("reasoning_signature");
        let assistant = message_at(&session, 1).verbatim().message();
        assert_eq!(assistant.role, Role::Assistant);
        assert!(matches!(
            &assistant.content[0],
            ContentBlock::Thinking { signature: Some(sig), .. } if sig == "sig-abc"
        ));
    }

    #[test]
    fn bedrock_native_signatures_convert_to_thinking_blocks() {
        let session = load_fixture("bedrock_signature");
        let assistant = message_at(&session, 1).verbatim().message();
        // Signed text and redacted payloads both convert; the provider-native
        // wrapper is gone, the signature payload stays.
        assert!(matches!(
            &assistant.content[0],
            ContentBlock::Thinking {
                signature: Some(_),
                ..
            }
        ));
        assert!(matches!(
            &assistant.content[1],
            ContentBlock::RedactedThinking { data } if data == "cmVkYWN0ZWQ="
        ));
    }

    #[test]
    fn parallel_tool_results_load_with_names() {
        let session = load_fixture("parallel_tool_results");
        let assistant = message_at(&session, 1).verbatim().message();
        let names: Vec<_> = ext::tool_calls(assistant)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["bash", "read"]);
        for (index, (id, name)) in [("call-1", "bash"), ("call-2", "read")].iter().enumerate() {
            let result = message_at(&session, 2 + index).verbatim().message();
            let ContentBlock::ToolResult {
                tool_use_id,
                name: result_name,
                ..
            } = &result.content[0]
            else {
                panic!("tool result block");
            };
            assert_eq!(tool_use_id, id);
            assert_eq!(result_name.as_deref(), Some(*name));
        }
    }

    #[test]
    fn image_tool_result_loads_as_inline_image() {
        let session = load_fixture("image_tool_result");
        let result = message_at(&session, 2).verbatim().message();
        let ContentBlock::ToolResult { content, name, .. } = &result.content[0] else {
            panic!("tool result block");
        };
        assert_eq!(name.as_deref(), Some("view_image"));
        // The fixture's result is image-only: a live `view_image` result
        // leads with its caption as a Text block, which converts verbatim.
        assert!(matches!(
            &content.0[0],
            ToolContent::Image { source: crate::history::ImageSource::Base64 { data, .. } }
                if data == "aGk="
        ));
    }

    #[test]
    fn json_tool_result_renders_to_text() {
        let session = load_fixture("json_tool_result");
        let result = message_at(&session, 2).verbatim().message();
        let ContentBlock::ToolResult { content, .. } = &result.content[0] else {
            panic!("tool result block");
        };
        let text = ext::tool_content_text(content);
        assert!(text.contains("\"matches\": 3"), "{text}");
        assert!(text.contains("\"files\""), "{text}");
    }

    #[test]
    fn marker_messages_convert_to_tagged_observations() {
        let session = load_fixture("markers");
        let texts = message_texts(session.messages());
        assert_eq!(
            texts,
            [
                "work",
                "[Cancelled by user]",
                "[Run failed: provider exploded]",
                "[The turn ended here; the run was cut short.]",
                crate::run::doom::GRACE_CALL_PROMPT,
                crate::storage::sessions::legacy_messages::LEGACY_NUDGE_PROMPT,
            ]
        );
        let tags: Vec<Option<&str>> = session
            .messages()
            .iter()
            .map(|entry| marker::of(entry.verbatim()))
            .collect();
        assert_eq!(
            tags,
            [
                None,
                Some(marker::CANCEL),
                Some(marker::FAILED),
                Some(marker::END),
                Some(marker::GRACE),
                Some(marker::NUDGE),
            ]
        );
        for entry in session.messages().iter().skip(1) {
            assert_eq!(
                entry.verbatim().message().kind,
                animist::history::MessageKind::Observation
            );
        }
    }

    #[test]
    fn subagent_histories_become_entry_lists() {
        let session = load_fixture("subagent");
        let sub = session
            .subagent_messages()
            .get("call-7")
            .expect("sub history");
        assert_eq!(sub.len(), 2);
        assert_eq!(message_texts(sub), ["do things", "child reply"]);
    }

    /// Load → save → load equals the first load, in the new shape.
    #[test]
    fn legacy_fixture_round_trips_through_the_new_format() {
        for name in [
            "text",
            "reasoning_signature",
            "bedrock_signature",
            "parallel_tool_results",
            "image_tool_result",
            "json_tool_result",
            "markers",
            "subagent",
        ] {
            let first = load_fixture(name);
            let tmp = TempDir::new().unwrap();
            let mut saved = first.clone();
            saved.save_to(tmp.path()).unwrap();

            // The rewrite is in the current format.
            let disk = fs::read_to_string(jsonl_path(tmp.path(), saved.id.id())).unwrap();
            assert!(
                disk.contains("\"t\":\"entry\""),
                "{name}: rewrite must use entry records"
            );
            assert!(
                !disk.contains("\"t\":\"msg\""),
                "{name}: no legacy msg records may remain"
            );

            let second = StoredSession::load_from(saved.id.id(), tmp.path()).unwrap();
            assert_eq!(
                second.messages(),
                first.messages(),
                "{name}: entries survive the round trip byte for byte"
            );
            let first_tags: Vec<_> = first
                .messages()
                .iter()
                .map(|e| marker::of(e.verbatim()).map(str::to_owned))
                .collect();
            let second_tags: Vec<_> = second
                .messages()
                .iter()
                .map(|e| marker::of(e.verbatim()).map(str::to_owned))
                .collect();
            assert_eq!(first_tags, second_tags, "{name}: tags survive");
            assert_eq!(
                second.subagent_messages().len(),
                first.subagent_messages().len(),
                "{name}: subagent histories survive"
            );
            // A saved legacy session no longer depends on the legacy reader:
            // every message is a source-only entry.
            for entry in second.messages() {
                assert!(entry.revisions().is_empty());
                assert!(entry.is_active());
            }
        }
    }

    /// Every converted fixture is wire-legal for the tools it references.
    #[test]
    fn converted_fixtures_pass_replay_validation() {
        let view: Vec<_> = load_fixture("parallel_tool_results")
            .messages()
            .iter()
            .map(|e| e.verbatim().message().clone())
            .collect();
        crate::run::view::assert_replay_valid(&view, &[]);

        let image_view: Vec<_> = load_fixture("image_tool_result")
            .messages()
            .iter()
            .map(|e| e.verbatim().message().clone())
            .collect();
        crate::run::view::assert_replay_valid(&image_view, &[]);
    }

    /// A user message whose text equals a marker is not a marker: identity
    /// is the tag, never the text.
    #[test]
    fn user_typed_marker_text_is_not_a_marker() {
        let typed = ext::entry(ext::stored_user_text("[Cancelled by user]"));
        assert_eq!(marker::of(typed.verbatim()), None);
        assert_eq!(
            typed.verbatim().message().kind,
            animist::history::MessageKind::Turn
        );
    }
}
