use super::{
    ARCHIVE_DIR, ARCHIVE_KEEP, ARCHIVE_MAX_BYTES, CWD_INDEX_FILE, DEFAULT_TITLE, LOG_BLOATED,
    MAX_APPENDS, MAX_TITLE_LEN, MSG_PREFIX, SESSION_VERSION, StoredSubagent, generate_title,
    json_path, jsonl_path, load_cwd_index, next_epoch, remove_from_cwd_index, update_cwd_index,
    write_full_session,
};
use super::{HistorySnapshot, Session, SessionError, SessionLog, StorageError, TitleSource};
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

type TestSession = Session<Value, Value, Value>;

const LEGACY_HEX_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const SONNET_COST: f64 = 0.42;
const HAIKU_COST: f64 = 0.08;

/// A turn that reports no price must not erase what was already billed.
#[test_case(None, None, None ; "unpriced_stays_unpriced")]
#[test_case(None, Some(SONNET_COST), Some(SONNET_COST) ; "first_price_starts_the_total")]
#[test_case(Some(SONNET_COST), Some(HAIKU_COST), Some(SONNET_COST + HAIKU_COST) ; "priced_turns_accumulate")]
#[test_case(Some(SONNET_COST), None, Some(SONNET_COST) ; "unpriced_turn_keeps_the_total")]
fn add_cost_only_grows_a_total(mut total: Option<f64>, addend: Option<f64>, expected: Option<f64>) {
    super::add_cost(&mut total, addend);
    assert_eq!(total, expected);
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

    let mut loaded = TestSession::load_from(id, tmp.path()).unwrap();
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

/// What a turn billed is written verbatim, read back verbatim, and keeps
/// adding up after a reload. A later unpriced turn must not throw away what
/// the earlier ones paid.
#[test]
fn recorded_costs_survive_a_reload_and_keep_adding_up() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
    session.add_model_usage("claude-sonnet-4", usage(100, Some(SONNET_COST)));
    session.add_model_usage("claude-haiku-4", usage(30, None));
    session.save_to(dir).unwrap();

    let on_disk = saved_usage_by_model(dir, session.id.id());
    assert_eq!(on_disk["claude-sonnet-4"]["cost"], Value::from(SONNET_COST));

    let mut loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    loaded.add_model_usage("claude-sonnet-4", usage(50, None));
    loaded.add_model_usage("claude-haiku-4", usage(10, Some(HAIKU_COST)));

    let sonnet = loaded.usage_by_model()["claude-sonnet-4"];
    assert_eq!((sonnet.input, sonnet.cost), (150, Some(SONNET_COST)));
    let haiku = loaded.usage_by_model()["claude-haiku-4"];
    assert_eq!((haiku.input, haiku.cost), (40, Some(HAIKU_COST)));
}

impl TitleSource for Value {
    fn first_user_text(&self) -> Option<&str> {
        if self.get("role")?.as_str()? != "user" {
            return None;
        }
        self.get("content")?.as_array()?.iter().find_map(|b| {
            if b.get("type")?.as_str()? == "text" {
                let text = b.get("text")?.as_str()?;
                (!text.is_empty()).then_some(text)
            } else {
                None
            }
        })
    }
}

fn user_message(text: &str) -> Value {
    text_message("user", text)
}

fn assistant_message(text: &str) -> Value {
    text_message("assistant", text)
}

fn text_message(role: &str, text: &str) -> Value {
    serde_json::json!({
        "role": role,
        "content": [{"type": "text", "text": text}]
    })
}

fn write_legacy_jsonl(path: &Path, session: &TestSession) {
    let mut file = std::fs::File::create(path).unwrap();
    write_full_session(&mut file, session).unwrap();
}

fn append_raw_msg(path: &Path, message: Value) {
    let record = serde_json::to_string(&serde_json::json!({"t":"msg","d": message})).unwrap();
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(record.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
}

#[test]
fn prune_orphans_drops_unreachable_tool_state() {
    fn ids(m: &Value) -> Vec<String> {
        vec![m.as_str().unwrap().to_owned()]
    }
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

    let mut session: TestSession = Session::new("model", "/p");
    session.push_message("task-live".into());
    session.set_subagent_messages("task-live".into(), vec!["sub-tool".into()]);
    session.set_subagent_messages("task-stale".into(), vec!["stale-sub-tool".into()]);
    session.set_subagents(vec![subagent("task-live"), subagent("task-stale")]);
    for id in ["task-live", "sub-tool", "stale-sub-tool", "orphan"] {
        session.insert_tool_output(id.into(), Arc::new(Value::Null));
    }

    session.prune_orphans(ids);

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
    let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/home/test/project");
    session.push_message(user_message("hello"));
    session.set_subagent_messages(
        "tool-1".into(),
        vec![user_message("sub-prompt"), assistant_message("sub-reply")],
    );
    session.save_to(dir).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
    assert_eq!(loaded.cwd, "/home/test/project");
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(loaded.version, SESSION_VERSION);
    assert_eq!(loaded.subagent_messages()["tool-1"].len(), 2);
}

#[test]
fn roundtrip_meta_session_rules_and_context_size() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.meta.session_rules = vec![PermissionRule {
        tool: ToolKey::native("bash"),
        scope: Some("ls".into()),
        effect: Effect::Allow,
    }];
    session.meta.context_size = 120_000;
    session.push_message(user_message("hi"));
    session.save_to(dir).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.meta.session_rules, session.meta.session_rules);
    assert_eq!(loaded.meta.context_size, 120_000);
}

#[test]
fn replacing_subagent_messages_diverges_the_log() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.set_subagent_messages("sub-1".into(), vec![user_message("old")]);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.set_subagent_messages("sub-1".into(), vec![user_message("new")]);
    let err = log.append(&session).unwrap_err();
    assert!(matches!(err, SessionError::LogDiverged { .. }));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.push_message(user_message("after"));
    log.append(&session).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.subagent_messages()["sub-1"].len(), 1);
    assert_eq!(loaded.subagent_messages()["sub-1"][0], user_message("new"));
}

#[test]
fn roundtrip_jsonl_incremental() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.push_message(assistant_message("reply"));
    session.push_message(user_message("second"));
    session.insert_tool_output(
        "tool-1".into(),
        Arc::new(serde_json::json!({"result": "ok"})),
    );
    session.set_subagent_messages("sub-1".into(), vec![user_message("sub-prompt")]);
    log.append(&session).unwrap();

    let mut sub1 = session.subagent_messages().get("sub-1").unwrap().to_vec();
    sub1.push(assistant_message("sub-reply"));
    session.set_subagent_messages("sub-1".into(), sub1);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.set_subagent_messages("sub-2".into(), vec![user_message("sub-2-prompt")]);
    log.append(&session).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 3);
    assert_eq!(loaded.tool_outputs().len(), 1);
    assert!(loaded.tool_outputs().contains_key("tool-1"));
    assert_eq!(loaded.subagent_messages()["sub-1"].len(), 2);
    assert_eq!(loaded.subagent_messages()["sub-2"].len(), 1);
}

#[test]
fn append_wrong_session_returns_id_mismatch() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let session_a: TestSession = Session::new("m", "/project");
    let session_b: TestSession = Session::new("m", "/project");
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
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    append_raw_msg(&jsonl_path(dir, session.id.id()), user_message("external"));

    session.push_message(user_message("second"));
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
    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
    assert_eq!(loaded.messages()[1], user_message("second"));
}

/// Past [`MAX_APPENDS`] the log is bloated with stale meta records and the
/// next save must go through a canonical rewrite, not another append.
#[test]
fn too_many_appends_forces_canonical_rewrite() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("seed"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    for i in 0..MAX_APPENDS {
        session.push_message(user_message(&format!("m{i}")));
        log.append(&session).unwrap();
    }
    let before = fs::read(jsonl_path(dir, session.id.id()))
        .unwrap()
        .split(|&b| b == b'\n')
        .count();
    session.push_message(user_message("one too many"));
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
    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), MAX_APPENDS + 2);
}

#[test]
fn crash_recovery_truncated_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("survives"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"t\":\"msg\",\"d\":{\"trun").unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);
}

#[test]
fn crash_recovery_skips_corrupt_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"CORRUPT_LINE\n").unwrap();
    drop(file);
    append_raw_msg(&path, user_message("after"));

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}

#[test]
fn rewind_compact() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    for i in 0..10 {
        session.push_message(user_message(&format!("msg-{i}")));
    }
    session.set_subagent_messages(
        "sub-1".into(),
        vec![user_message("sub-prompt"), assistant_message("sub-reply")],
    );
    let _ = SessionLog::rewrite(dir, &session).unwrap();

    session.truncate_messages(5);
    session.prune_orphans(|_| Vec::new());
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    session.push_message(user_message("after-compact-1"));
    session.push_message(user_message("after-compact-2"));
    session.push_message(user_message("after-compact-3"));
    log.append(&session).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 8);
    assert!(loaded.subagent_messages().is_empty());
}

#[test]
fn migration_json_to_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("legacy"));

    let json_path = json_path(dir, session.id.id());
    fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
    update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);

    let _log = SessionLog::rewrite(dir, &loaded).unwrap();

    assert!(!json_path.exists());
    assert!(jsonl_path(dir, session.id.id()).exists());

    let reloaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(reloaded.messages().len(), 1);
    assert_eq!(reloaded.model, "m");
}

#[test]
fn load_nonexistent_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let id = CraftId::generate();
    let err = TestSession::load_from(id, tmp.path()).unwrap_err();
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
    let mut session: TestSession = Session::new("m", "/project");
    session.id = id.into();
    session.push_message(user_message("legacy"));
    let legacy_path = dir.join(format!("{legacy}.jsonl"));
    write_legacy_jsonl(&legacy_path, &session);

    let loaded = TestSession::load_from(id, dir).unwrap();
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
    let mut s1: TestSession = Session::new("m", "/project-a");
    let mut s2: TestSession = Session::new("m", "/project-b");
    let mut s3: TestSession = Session::new("m", "/project-a");
    s1.save_to(dir).unwrap();
    s2.save_to(dir).unwrap();
    s3.save_to(dir).unwrap();

    let list = TestSession::list_in(Some("/project-a"), dir).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s.id != s2.id));
}

fn save_with_time(session: &mut TestSession, dir: &Path, time: u64) {
    session.updated_at = time;
    SessionLog::rewrite(dir, session).unwrap();
    update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();
}

#[test]
fn latest_returns_most_recent_for_cwd() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s1: TestSession = Session::new("m", "/project");
    s1.title = "first".into();
    save_with_time(&mut s1, dir, 1000);

    let mut s2: TestSession = Session::new("m", "/other");
    save_with_time(&mut s2, dir, 2000);

    let mut s3: TestSession = Session::new("m", "/project");
    s3.title = "latest".into();
    save_with_time(&mut s3, dir, 3000);

    let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
    assert_eq!(latest.title, "latest");
}

#[test]
fn latest_falls_back_when_index_stale() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.save_to(dir).unwrap();

    let index_path = dir.join(CWD_INDEX_FILE);
    let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
    fs::write(&index_path, serde_json::to_vec(&stale).unwrap()).unwrap();

    let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
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
    let messages: Vec<Value> = if input.is_empty() {
        vec![]
    } else {
        vec![user_message(input)]
    };
    assert_eq!(generate_title(&messages), expected);
}

#[test]
fn dirty_persisted_title_normalized_on_list_and_load() {
    const NORMALIZED: &str = "line one line two";
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s: TestSession = Session::new("m", "/project");
    s.push_message(user_message("hi"));
    let mut log = SessionLog::rewrite(dir, &s).unwrap();
    s.set_title("line one\n\n\tline two".into());
    log.append(&s).unwrap();

    let list = TestSession::list_in(Some("/project"), dir).unwrap();
    assert_eq!(list[0].title, NORMALIZED);
    assert_eq!(
        TestSession::load_from(s.id.id(), dir).unwrap().title,
        NORMALIZED
    );
}

#[test]
fn delete_removes_file_and_cwd_index() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut s1: TestSession = Session::new("m", "/project");
    s1.save_to(dir).unwrap();
    let mut s2: TestSession = Session::new("m", "/other");
    s2.save_to(dir).unwrap();

    TestSession::delete_from(s1.id.id(), dir).unwrap();
    assert!(!jsonl_path(dir, s1.id.id()).exists());
    let index = load_cwd_index(dir);
    assert!(!index.values().any(|v| *v == s1.id.to_string()));
    assert_eq!(index.get("/other"), Some(&s2.id.to_string()));
}

#[test]
fn delete_nonexistent_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let id = CraftId::generate();
    let err = TestSession::delete_from(id, tmp.path()).unwrap_err();
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
    let mut session: TestSession = Session::new("m", "/project");
    session.id = id.into();
    session.push_message(user_message("legacy"));
    let legacy_path = dir.join(format!("{legacy}.jsonl"));
    write_legacy_jsonl(&legacy_path, &session);

    TestSession::delete_from(id, dir).unwrap();
    assert!(!legacy_path.exists());
    let canonical = jsonl_path(dir, id);
    assert!(!canonical.exists());
}

#[test]
fn delete_removes_coexisting_json_and_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("hi"));

    let jsonl_file = jsonl_path(dir, session.id.id());
    write_legacy_jsonl(&jsonl_file, &session);
    let json_file = json_path(dir, session.id.id());
    fs::write(&json_file, serde_json::to_vec(&session).unwrap()).unwrap();

    TestSession::delete_from(session.id.id(), dir).unwrap();
    assert!(!jsonl_file.exists());
    assert!(!json_file.exists());
}

#[test]
fn load_picks_jsonl_when_legacy_dual_file_exists() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut jsonl_session: TestSession = Session::new("m", "/project");
    jsonl_session.id = id.into();
    jsonl_session.push_message(user_message("newer"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

    let mut json_session: TestSession = Session::new("m", "/project");
    json_session.id = id.into();
    json_session.push_message(user_message("older"));
    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

    let loaded = TestSession::load_from(id, dir).unwrap();
    assert_eq!(loaded.messages().len(), 1);
    assert_eq!(loaded.messages()[0], user_message("newer"));
}

#[test]
fn load_dual_legacy_files_does_not_leave_duplicate_in_list() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut jsonl_session: TestSession = Session::new("m", "/project");
    jsonl_session.id = id.into();
    jsonl_session.push_message(user_message("newer"));
    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

    let mut json_session: TestSession = Session::new("m", "/project");
    json_session.id = id.into();
    json_session.push_message(user_message("older"));
    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

    TestSession::load_from(id, dir).unwrap();

    assert!(!legacy_json.exists(), "legacy .json sibling left behind");
    let list = TestSession::list_in(Some("/project"), dir).unwrap();
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
    let mut session: TestSession = Session::new("m", "/project");
    session.id = id.into();
    session.push_message(user_message("legacy"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_legacy_jsonl(&legacy_jsonl, &session);

    let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
    fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

    TestSession::delete_from(id, dir).unwrap();
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
        .filter(|line| line.starts_with(MSG_PREFIX))
        .count()
}

#[test]
fn rewrite_dropping_messages_archives_the_old_file() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("model", "/p");
    for i in 0..5 {
        session.push_message(user_message(&format!("turn {i}")));
    }
    session.save_to(dir).unwrap();

    session.replace_messages(vec![user_message("summary")]);
    session.save_to(dir).unwrap();

    let live = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(live.messages().len(), 1);
    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), 1);
    assert_eq!(msg_line_count(&archives[0]), 5);
}

#[test]
fn rewrite_without_shrink_does_not_archive() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("model", "/p");
    session.push_message(user_message("one"));
    session.save_to(dir).unwrap();

    session.push_message(user_message("two"));
    session.save_to(dir).unwrap();

    assert!(!archive_dir_for(dir, session.id.id()).exists());
}

#[test]
fn archived_file_round_trips() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("model", "/p");
    let pre: Vec<Value> = (0..3).map(|i| user_message(&format!("turn {i}"))).collect();
    for msg in &pre {
        session.push_message(msg.clone());
    }
    session.save_to(dir).unwrap();

    session.replace_messages(vec![assistant_message("summary")]);
    session.save_to(dir).unwrap();

    let scratch = TempDir::new().unwrap();
    let archives = archive_paths(dir, session.id.id());
    assert_eq!(archives.len(), 1);
    fs::copy(
        &archives[0],
        scratch.path().join(format!("{}.jsonl", session.id.id())),
    )
    .unwrap();
    let archived = TestSession::load_from(session.id.id(), scratch.path()).unwrap();
    assert_eq!(archived.messages(), pre.as_slice());
}

#[test]
fn archive_retention_keeps_newest_three() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("model", "/p");
    session.push_message(user_message("seed"));
    session.save_to(dir).unwrap();
    for round in 1..=5 {
        for _ in 0..round {
            session.push_message(user_message(&format!("turn {round}")));
        }
        session.save_to(dir).unwrap();
        session.replace_messages(vec![user_message(&format!("summary {round}"))]);
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
    let mut session: TestSession = Session::new("model", "/p");
    session.push_message(user_message("one"));
    session.push_message(user_message("two"));
    session.save_to(dir).unwrap();

    let archive_dir = archive_dir_for(dir, session.id.id());
    fs::create_dir_all(&archive_dir).unwrap();
    let existing = archive_dir.join(format!("{EXISTING_ARCHIVE_SEQ}.jsonl"));
    fs::write(&existing, "").unwrap();

    session.replace_messages(vec![user_message("summary")]);
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
    let mut session: TestSession = Session::new("model", "/p");
    session.push_message(user_message("one"));
    session.push_message(user_message("two"));
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

    session.replace_messages(vec![user_message("summary")]);
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
    let mut session: TestSession = Session::new("model", "/p");
    session.push_message(user_message("one"));
    session.push_message(user_message("two"));
    session.save_to(dir).unwrap();
    session.replace_messages(vec![user_message("summary")]);
    session.save_to(dir).unwrap();
    let archive_dir = archive_dir_for(dir, session.id.id());
    assert!(archive_dir.exists());

    TestSession::delete_from(session.id.id(), dir).unwrap();
    assert!(!archive_dir.exists());
    assert!(!jsonl_path(dir, session.id.id()).exists());
}

#[test]
fn rewrite_removes_legacy_named_files() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
    let mut session: TestSession = Session::new("m", "/project");
    session.id = id.into();
    session.push_message(user_message("legacy"));

    let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
    write_legacy_jsonl(&legacy_jsonl, &session);

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

    let mut newest: TestSession = Session::new("m", "/project");
    newest.title = "newest".into();
    save_with_time(&mut newest, dir, 3000);

    let mut older: TestSession = Session::new("m", "/project");
    older.title = "older".into();
    older.updated_at = 1000;
    let json_path = json_path(dir, older.id.id());
    fs::write(&json_path, serde_json::to_vec(&older).unwrap()).unwrap();

    // Opening the older session migrates it to canonical jsonl, but must not
    // repoint cwd->latest at it.
    let loaded = TestSession::load_from(older.id.id(), dir).unwrap();
    assert_eq!(loaded.title, "older");
    assert!(!json_path.exists());

    let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
    assert_eq!(latest.title, "newest");
}

#[test]
fn load_surfaces_corrupt_header_id() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let id = CraftId::generate();
    let mut session: TestSession = Session::new("m", "/project");
    session.id = id.into();

    let path = jsonl_path(dir, id);
    write_legacy_jsonl(&path, &session);

    let corrupted =
        fs::read_to_string(&path)
            .unwrap()
            .replacen(&id.to_string(), "not-a-valid-id", 1);
    fs::write(&path, corrupted).unwrap();

    let err = TestSession::load_from(id, dir).unwrap_err();
    assert!(matches!(err, SessionError::CorruptHeaderId { .. }));
}

#[test]
fn remove_from_cwd_index_matches_legacy_hex_value() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let legacy = "550e8400-e29b-41d4-a716-446655440000";
    let id: CraftId = legacy.parse().unwrap();
    let mut session: TestSession = Session::new("m", "/project");
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
    let title = generate_title(&[user_message(&input)]);
    assert!(title.len() <= MAX_TITLE_LEN * 4);
    assert!(title.is_char_boundary(title.len()));
}

#[test]
fn scan_headers_reads_both_formats() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut s1: TestSession = Session::new("m", "/project");
    s1.title = "jsonl-session".into();
    s1.save_to(dir).unwrap();

    let mut s2: TestSession = Session::new("m", "/project");
    s2.title = "json-session".into();
    let json_path = json_path(dir, s2.id.id());
    fs::write(&json_path, serde_json::to_vec(&s2).unwrap()).unwrap();

    let list = TestSession::list_in(Some("/project"), dir).unwrap();
    assert_eq!(list.len(), 2);
}

#[test]
fn scan_jsonl_reads_trailing_meta_from_tail() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let mut session: TestSession = Session::new("m", "/project");
    session.save_to(dir).unwrap();
    let path = jsonl_path(dir, session.id.id());

    let header_line = fs::read_to_string(&path).unwrap();
    let mut buf = header_line;
    for i in 0..5000u64 {
        buf.push_str(&format!("{{\"t\":\"out\",\"id\":\"tool-{i}\"}}\n"));
    }
    buf.push_str("{\"t\":\"meta\",\"title\":\"final-title\",\"updated_at\":99999}\n");
    fs::write(&path, buf).unwrap();

    let list = TestSession::list_in(Some("/project"), dir).unwrap();
    let summary = list.iter().find(|s| s.id == session.id).unwrap();
    assert_eq!(summary.title, "final-title");
    assert_eq!(summary.updated_at, 99999);
}

#[test]
fn load_wrong_version_legacy_returns_error() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("test/model", "/tmp");
    session.version = 999;
    let path = json_path(dir, session.id.id());
    fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

    let err = TestSession::load_from(session.id.id(), dir).unwrap_err();
    assert!(matches!(
        err,
        SessionError::VersionMismatch { found: 999, .. }
    ));
}

#[test]
fn open_roundtrip_resumes_append() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));

    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    session.push_message(assistant_message("reply"));
    log.append(&session).unwrap();
    drop(log);

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);

    session.push_message(user_message("second"));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    log.append(&session).unwrap();
    drop(log);

    let reloaded = TestSession::load_from(session.id.id(), dir).unwrap();
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

    let err = TestSession::load_from(id, dir).unwrap_err();
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
    let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
    let run = HistorySnapshot {
        epoch: next_epoch(),
        messages: Arc::new(vec![user_message("hi")]),
    };
    let meta = session.meta.clone();
    Session::checkpoint(&mut session, Some(&run), meta.clone(), Value::Null);
    Arc::make_mut(&mut session).set_subagent_messages("sub-1".into(), vec![user_message("old")]);
    let mut log = SessionLog::rewrite(dir, &session).unwrap();

    Arc::make_mut(&mut session).set_subagent_messages("sub-1".into(), vec![user_message("new")]);
    let advanced = HistorySnapshot {
        epoch: run.epoch,
        messages: Arc::new(vec![user_message("hi"), assistant_message("reply")]),
    };
    Session::checkpoint(&mut session, Some(&advanced), meta, Value::Null);
    log.append(&session)
        .expect_err("append must be refused after a local rewrite");

    SessionLog::rewrite(dir, &session).unwrap();
    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), session.messages().len());
    assert_eq!(
        loaded.subagent_messages().get("sub-1").map(|m| m.len()),
        Some(1)
    );
    assert!(
        loaded.subagent_messages()["sub-1"][0]
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            == Some("new")
    );
}

#[test]
fn crash_recovery_preserves_tool_outputs_around_corrupt_line() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));
    session.insert_tool_output("t1".into(), Arc::new(serde_json::json!({"result": "ok"})));
    let mut log = SessionLog::rewrite(dir, &session).unwrap();
    log.append(&session).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"CORRUPT\n").unwrap();
    file.write_all(
        serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
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

    let err = TestSession::load_from(id, dir).unwrap_err();
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
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("msg"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"\n\n\n").unwrap();
    file.write_all(
        serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("after")}))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}

#[test]
fn unknown_record_type_is_skipped() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let mut session: TestSession = Session::new("m", "/project");
    session.push_message(user_message("first"));
    session.save_to(dir).unwrap();

    let path = jsonl_path(dir, session.id.id());
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"t\":\"future_type\",\"d\":{}}\n")
        .unwrap();
    file.write_all(
        serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
}
