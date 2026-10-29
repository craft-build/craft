# 12 — Cleanup, documentation, conformance, followups, async audit

## Required workflow

- **Load the `rust-best-practices` skill before writing any code** and apply it
  to every Rust change in this prompt.
- Follow [README → Instructions for every prompt](README.md#instructions-for-every-prompt).
- When implementation is finished, **run the review agent**
  (`.craft/agents/reviewer.md`) as described in [Finish](#finish).

## Goal

Finish the migration: remove leftovers, update the docs that still describe
rig, prove provider conformance without paid calls, confirm the harness is
async native, and confirm no workaround for a released animist followup
survived.

## Work

1. **Leftovers.**
   - Search for rig and its idioms:
     `grep -rni 'rig\b\|rig-core\|rig_core\|DynamicModel\|CompletionSource\|edge::'`
     across `agent/` and `docs/`.
   - Update module docs that cite rig. Many do: `subagent.rs` ("Rig owns the
     loop"), `models_dev.rs`, `model_registry.rs`, `auto_review.rs`,
     `knowledge_memory.rs`, `tools/mod.rs`, `providers/bedrock.rs`, and others.
   - Delete dead code, plus `#[allow(dead_code)]` forward-substrate variants
     that animist now covers.
   - Remove workspace dependencies nothing uses any more. animist owns Bedrock
     credentials, signing, transport and listing (02), so expect
     `aws-sdk-bedrock`, `aws-sdk-bedrockruntime`, `aws-config`,
     `aws-credential-types` and `aws-smithy-types` to go unless a test seam
     still needs one (keep those as dev-dependencies). Check with
     `cargo machete`, or `cargo +nightly udeps` if available, otherwise by
     hand.
2. **User documentation.**
   - Update `agent/README.md` and `agent/agent.example.bml`, plus any
     provider/config docs, covering:
     - the supported kinds and the new kinds (`synthetic`, `zai-coding-plan`,
       `llama-cpp`, `omlx`);
     - the dropped kinds, with migration snippets (`openai-compatible` +
       `base_url`);
     - the Bedrock protocol table (Messages, Responses, Chat Completions,
       Converse), the per-model override, and the Converse thinking caveat;
     - the session format change and the no-downgrade note.
   - Add a CHANGELOG/release note entry if the repo keeps one.
3. **Conformance tests.** Extend or create the suite hardening/25 describes. One
   table-driven test per supported kind, against local mock HTTP servers that
   speak each protocol's SSE (reuse animist's test fixtures as a reference, not
   a dependency). Assert:
   - text, thinking and tool-call round trips through the full
     `Conversation`/layers stack;
   - tool-result images for Anthropic/Responses, and relocation for Chat
     Completions;
   - 429/401/5xx/overflow handling through `RecoveryLayer`, including
     Bedrock's in-provider 429/503 retries not stacking with craft backoff;
   - usage, and pricing where a catalog provides it.
4. **Opt-in live smoke tests.** Add `#[ignore]` live tests, or an example binary,
   gated by env vars, mirroring animist's `docs/manual-provider-tests.md`.
   Document how to run them. Never run them in CI or without explicit approval.
5. **Followups status.** animist 0.7.0 released F01–F07, F09 and F10, and the
   prompts used them directly. Confirm none of the old workarounds exist:
   `grep -rn 'catch_unwind\|tool_name_for\|needs animist F0' agent/src`
   is empty, there is no persistence-time revision pruning, no snapshot-shape
   diffing in session sync, no one-shot-agent tool harness, no
   `with_client`/`max_attempts`/`SdkConfig` handling for Bedrock, and no
   driver re-run for grace, nudge or `MaxTokens`.
   - F08 (usage cost) is still open in animist. If a later animist release
     ships it, price from `TokenUsage::cost` and keep `usage.rs` only for
     craft-specific schedules; otherwise leave craft's pricing in its single
     function and say so in the completion note.
   - New gaps that 02–11 recorded become followup prompts in animist's
     `docs/followups/`, in the existing format.
6. **Async audit** (README → Async native):
   - `grep -rn 'block_in_place\|block_on\|new_current_thread\|futures::executor' agent/src`
     has no hits except the `main.rs` runtime.
   - List every remaining `spawn_blocking` with its reason, and confirm each
     is CPU-bound or a sync-only library.
   - `clippy::await_holding_lock` is clean.
7. **Performance sanity.** Measure startup time, one scripted 50-tool-call
   session, and session save/load times for a large fixture against the 01
   baseline commit, and record them. Investigate any regression above 20%.

## Acceptance

- No rig references remain anywhere in `agent/` or `docs/`, except historical
  notes in `docs/hardening/` and this directory.
- The conformance suite covers every supported kind and passes offline.
- No workaround for a released animist followup remains, and any new gap
  has an animist followup prompt.
- The async audit is clean.
- Final: `cargo fmt -p craft --check`, `cargo clippy -p craft --all-targets`
  (no warnings beyond the 01 baseline, and ideally fewer), and
  `cargo test -p craft` all green.

## Finish

1. Run fmt, clippy and the tests (see the README).
2. **Run the review agent** (`.craft/agents/reviewer.md`) over this prompt's
   full diff, **and once more over the entire migration branch diff against
   `main`**. Address its findings, re-run validation, and repeat until the
   verdict is clean.
3. Append a completion note: performance numbers, conformance coverage, and the
   final test count compared with the 01 baseline.

## Completion note

_(fill in when done)_
