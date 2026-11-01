use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use animist::history::{ContentBlock, History};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::{Error, Result, definition, finish, invalid, text_output};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetrieveArgs {
    /// Hash of the compressed content to retrieve, taken from a
    /// `[N lines compressed from M. Retrieve original: hash=HASH]` marker.
    pub hash: String,
}

#[derive(Debug)]
pub struct RetrieveOutput {
    pub text: String,
}

impl super::error::IntoToolResult for RetrieveOutput {
    fn into_tool_result(self) -> ToolOutput {
        text_output(self.text)
    }
}

/// Restores the original text of a replaced tool output by content hash.
/// The originals live in history entry sources (08): every entry keeps its
/// verbatim message even after compression alterations or compaction
/// removed it from the active view, so retrieval works in a later turn of
/// the same conversation and after a session reload.
#[derive(Clone)]
pub struct Retrieve(pub History);

impl Retrieve {
    /// The original whose hash matches, searched across every entry's
    /// source and revisions (inactive entries included). `Err` names an
    /// ambiguous short hash (two distinct originals colliding) instead of
    /// silently serving the older one: the pre-08 store extended the hash
    /// at write time, which the marker writers no longer do, so a
    /// collision must fail loudly rather than hand the model the wrong
    /// output to work from.
    async fn resolve(&self, hash: &str) -> std::result::Result<String, ResolveError> {
        // The snapshot is a point-in-time copy; the history lock is held
        // only while it is cloned.
        let entries = self.0.snapshot().await;
        let mut matched: Option<String> = None;
        for entry in entries.entries() {
            let candidates = std::iter::once(entry.verbatim()).chain(entry.revisions());
            for stored in candidates {
                for block in crate::history::ext::tool_results(stored.message()) {
                    let ContentBlock::ToolResult {
                        content, is_error, ..
                    } = block
                    else {
                        continue;
                    };
                    if *is_error {
                        continue;
                    }
                    let original = crate::history::ext::tool_content_text(content);
                    if crate::compression::content_hash(&original) == hash
                        || crate::compression::extended_content_hash(&original) == hash
                    {
                        fold_match(&mut matched, original, hash)?;
                    }
                }
            }
        }
        matched.ok_or(ResolveError::Missing {
            hash: hash.to_owned(),
        })
    }

    async fn execute(&self, args: RetrieveArgs) -> Result<RetrieveOutput> {
        match self.resolve(&args.hash).await {
            Ok(original) => Ok(RetrieveOutput { text: original }),
            Err(ResolveError::Missing { hash }) => Err(invalid(format!(
                "no content found for hash={hash}. The original may have been removed from the \
                 session history."
            ))),
            Err(ResolveError::Ambiguous { hash }) => Err(invalid(format!(
                "hash={hash} matches multiple distinct outputs (a rare short-hash collision). \
                 Re-read the source you need instead of retrieving by this hash."
            ))),
        }
    }
}

/// Why a hash did not resolve to exactly one original.
#[derive(Debug)]
enum ResolveError {
    Missing { hash: String },
    Ambiguous { hash: String },
}

/// Fold one hash-matching original into the resolution: a second match
/// with different content is a collision, not a resolution. (Extracted so
/// the decision is testable without manufacturing a real 64-bit hash
/// collision.)
fn fold_match(
    matched: &mut Option<String>,
    original: String,
    hash: &str,
) -> std::result::Result<(), ResolveError> {
    match matched {
        Some(existing) if *existing != original => Err(ResolveError::Ambiguous {
            hash: hash.to_owned(),
        }),
        _ => {
            *matched = Some(original);
            Ok(())
        }
    }
}

const RETRIEVE_DESCRIPTION: &str = "Retrieve the original (uncompressed) content for a previously compressed tool output. \
     Use the hash value from a compression marker in the conversation. Markers appear as \
     [N lines compressed from M. Retrieve original: hash=HASH].";

#[async_trait]
impl ToolImpl for Retrieve {
    type Args = RetrieveArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<RetrieveArgs>("retrieve", RETRIEVE_DESCRIPTION)
    }

    // The history is read through its own snapshot, so retrieval is
    // independent of every other call.

    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    async fn call(&self, _context: &ToolContext<'_>, args: RetrieveArgs) -> Result<ToolOutput> {
        finish(self.execute(args).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;

    fn args(hash: &str) -> RetrieveArgs {
        RetrieveArgs {
            hash: hash.to_owned(),
        }
    }

    async fn invoke(tool: &Retrieve, args: RetrieveArgs) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .build()
            .expect("harness");
        harness
            .call(tool.clone(), serde_json::json!({ "hash": args.hash }))
            .await
            .expect("retrieve outcome")
    }

    /// A history seeded with one tool-result entry.
    fn history_with_result(text: &str) -> History {
        History::from_entries(ext::entries(vec![ext::stored(ext::observation(vec![
            ext::tool_result("t1", Some("bash"), text, false),
        ]))]))
    }

    #[tokio::test]
    async fn returns_the_original_matching_the_hash() {
        let original = "the original text";
        let history = history_with_result(original);
        let tool = Retrieve(history);
        let hash = crate::compression::content_hash(original);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            original
        );
    }

    #[tokio::test]
    async fn reads_the_source_under_a_compression_revision() {
        // The durable compression pass revised the entry; the marker's hash
        // still resolves to the verbatim source.
        let original = "the original text";
        let history = history_with_result(original);
        let hash = crate::compression::content_hash(original);
        crate::compaction::alter::run_passes(
            &history,
            &crate::compression::CompressionConfig {
                enabled: false, // only the read-lifecycle pass can run here
                ..Default::default()
            },
            None,
        )
        .await
        .expect("passes");
        let tool = Retrieve(history);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            original
        );
    }

    /// A history whose compacted-away result entry is inactive: the
    /// original must still resolve (compaction deactivates, never deletes).
    #[tokio::test]
    async fn returns_the_original_after_compaction_removed_the_entry() {
        let original = "the compacted-away original text";
        let mut messages = vec![
            crate::compaction::test_support::stored_assistant_tool_args(
                "t1",
                "bash",
                serde_json::json!({"command": "cat file"}),
            ),
            ext::stored(ext::observation(vec![ext::tool_result(
                "t1",
                Some("bash"),
                original,
                false,
            )])),
        ];
        // A long tail so the VCC stage has a boundary to cut at.
        for i in 0..6 {
            messages.push(crate::compaction::test_support::stored_user(&format!(
                "task {i} with a fairly long instruction"
            )));
            messages.push(crate::compaction::test_support::stored_assistant_text(
                &format!("working on task {i}"),
            ));
        }
        let history = History::from_entries(ext::entries(messages));
        let hash = crate::compression::content_hash(original);
        let engine =
            crate::compaction::CompactionEngine::new(vec![crate::config::CompactionConfig {
                kind: crate::config::CompactionKind::Vcc,
                context: 0.6,
            }]);
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::compaction::CompactionState::default(),
        ));
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("summary", 1_000_000, 8_192, false, false),
        ));
        let (sink, _stream) = crate::harness::EventSink::new();
        assert!(
            engine
                .force_compact(&state, &model, &history, Some(1_000), &sink)
                .await,
            "the stage compacts the head"
        );
        let tool = Retrieve(history);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            original
        );
    }

    /// Entry sources and revisions survive a serialization round trip (the
    /// persister writes entry records), so retrieval works after a session
    /// reload.
    #[tokio::test]
    async fn returns_the_original_after_a_session_reload_roundtrip() {
        let original = "the persisted original text";
        let mut messages = vec![
            crate::compaction::test_support::stored_assistant_tool_args(
                "t1",
                "bash",
                serde_json::json!({"command": "cat file"}),
            ),
            ext::stored(ext::observation(vec![ext::tool_result(
                "t1",
                Some("bash"),
                original,
                false,
            )])),
        ];
        messages.push(crate::compaction::test_support::stored_user("next turn"));
        let history = History::from_entries(ext::entries(messages));
        // Compress durably first, so the reload carries source + revision.
        crate::compaction::alter::run_passes(
            &history,
            &crate::compression::CompressionConfig::default(),
            None,
        )
        .await
        .expect("passes");
        let entries = history.snapshot().await.entries().to_vec();
        let json = serde_json::to_string(&entries).expect("entries serialize");
        let restored: Vec<_> = serde_json::from_str(&json).expect("entries deserialize");
        let restored = History::from_entries(restored);
        let hash = crate::compression::content_hash(original);
        let tool = Retrieve(restored);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            original,
            "the reloaded history resolves the marker's hash to the source"
        );
    }

    /// Duplicate *identical* results (a dedup replay) share one hash and
    /// one original: not ambiguous.
    #[tokio::test]
    async fn identical_duplicates_resolve_to_the_shared_original() {
        let original = "the shared original text";
        let block = ext::tool_result("t1", Some("bash"), original, false);
        let history = History::from_entries(ext::entries(vec![
            ext::stored(ext::observation(vec![block.clone()])),
            ext::stored(ext::observation(vec![block])),
        ]));
        let hash = crate::compression::content_hash(original);
        let tool = Retrieve(history);
        let output = invoke(&tool, args(&hash)).await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            original
        );
    }

    /// The collision decision: two distinct originals matching one hash
    /// error; identical duplicates (a dedup replay) fold to one original.
    /// (`content_hash` is full-width hex, so a real collision cannot be
    /// manufactured cheaply — the fold is the seam.)
    #[test]
    fn distinct_matching_originals_are_ambiguous_identical_fold() {
        let mut matched = None;
        fold_match(&mut matched, "original a".to_owned(), "abababab").unwrap();
        assert_eq!(matched.as_deref(), Some("original a"));
        // The dedup replay: same content again is not a collision.
        fold_match(&mut matched, "original a".to_owned(), "abababab").unwrap();
        assert_eq!(matched.as_deref(), Some("original a"));
        // A second distinct original under the same hash is.
        let error =
            fold_match(&mut matched, "original b".to_owned(), "abababab").expect_err("ambiguity");
        assert!(matches!(error, ResolveError::Ambiguous { hash } if hash == "abababab"));
    }

    #[tokio::test]
    async fn unknown_hash_is_an_error() {
        let tool = Retrieve(History::new());
        let output = invoke(&tool, args("deadbeef")).await;
        assert!(output.is_error, "expected an error result");
        let text = crate::history::ext::tool_content_text(&output.content);
        assert!(
            text.contains("no content found for hash=deadbeef"),
            "{text}"
        );
    }
}
