# Feature spec: Post-turn memory auto-extraction

Status: implementation specification, ready to implement. Part of the clark-code
feature review (see `docs/clark-code-review-assessment.md`).

## 0. Summary

Craft's `memory` plugin only saves a fact when the model chooses to call the
`memory` tool mid-task. That is unreliable: the model is busy solving the task
and frequently forgets to persist decisions, preferences, and gotchas the user
stated in passing. clark-code's evals showed in-loop memory tool use is "a coin
flip," and their structural fix was to extract durable facts *after* the turn
finishes, off the latency path.

This spec adds a post-turn hook that, after a run ends, runs a cheap keyword
pre-filter on the user's message and, if it fires, makes one side-completion to
extract up to N durable facts as structured JSON, then writes/supersedes them
into the existing per-project memory directory. Best-effort: failures never
block a turn or surface to the user.

## 1. Why this matters

Memory only helps if it gets written. Today craft relies on the model
remembering to call `memory` while it is also reading files, editing, and
running tests. The facts most worth saving (a stated preference, an architecture
decision, a "from now on do X") are exactly the ones the model is least likely
to pause for. Moving extraction to a dedicated post-turn side-completion, with
the existing memory titles passed in so it can supersede rather than duplicate,
structurally fixes the gap without depending on in-loop discipline.

This fits craft cleanly because craft's memory is already open and local
(per-project markdown under `<state>/projects/<id>/memories`, surfaced via an
`after_instructions` prompt hint) — the same shape as clark's `provider-local`
memory. No cloud, no new storage.

## 2. Reference: how clark-code does it

- **Keyword pre-filter off the latency path.** clark
  `crates/provider-local/src/memory_extraction.rs:35-73` (`worth_extracting`)
  lowercases the user text and checks ~30 durable-fact cues
  (`"i'm "`, `"my product"`, `"we decided"`, `"from now on"`, `"rebrand"`, etc.)
  to skip pure task traffic and avoid spending a model call on messages with no
  durable content.
- **Supersession-aware extraction.** clark
  `crates/provider-local/src/memory_extraction.rs:100-246` (`extract_and_store`)
  loads existing project + global memory titles (`memory::load_facts`) and
  passes them to the model so it can mark *supersessions* rather than duplicate.
  A small side-completion (`ctx.llm.complete`) with a strict JSON-only system
  prompt (`:129-144`) extracts up to `MAX_FACTS = 4` facts, each
  `{title, content, scope, type, source, supersedes}`.
- **Deterministic write + supersede.** Code then routes each fact to
  `memory::memory_dir(project_root)` = `<root>/.clark/memory/` per
  `crates/provider-local/src/memory.rs:21,79-81`, tags provenance
  (`user-stated`/`inferred`), and if `supersedes` is non-empty, deletes the old
  note via `memory::delete_memory` (trying both scopes, `:199-244`).
- **Tolerant parsing.** `parse_extraction` (`:249-253`) slices first `{` to last
  `}` so prose or code fences around the JSON do not break it.
- **Best-effort throughout.** Failures are silent; extraction never blocks a
  turn.

## 3. Where this lives in craft

| Concern | Location |
|---|---|
| Run-end turn boundary (the hook point) | `craft-agent/src/agent/run.rs:570-595` — `TurnOutcome::Done(stop_reason)`, after `run_advisor()` (`run.rs:572`) and before `emit_done` (`run.rs:593`) |
| Memory storage (existing) | `plugins/memory/init.lua:5-27` — `<state>/projects/<project_id>/memories` via `helpers.project_id(root)`; `craft.env.state_dir()` |
| Memory list/index (existing) | `plugins/memory/init.lua:29-62` prompt hint reads the dir; `memory_helpers` (`collect_file_entries`, `MAX_HINT_*`) |
| Provider side-completion | `craft-providers/src/provider.rs:274` (`Provider::stream_message`); reuse a weak-tier `Model` like the advisor does |
| Model role for extraction | `model_roles.toml` — add a `memory_extractor` role (weak tier), mirroring how `advisor` is configured |
| New module | `craft-agent/src/agent/memory_extraction.rs` (logic + parsing + tests), wired from `run.rs` |

## 4. Implementation

### 4.1 The hook point

In `craft-agent/src/agent/run.rs`, the `TurnOutcome::Done` arm
(`run.rs:570-595`) is the natural post-run boundary. After the advisor runs
(`run.rs:572`) and before `emit_done` (`run.rs:593`), spawn the extraction as a
detached task so it never blocks the run's return or the host's re-prompt:

```rust
TurnOutcome::Done(stop_reason) => {
    self.snapshot.commit();
    let note = self.run_advisor().await;
    // ... existing advisor_action handling ...
    if matches!(self.mode, AgentMode::Flow(_)) {
        self.commit_turn_write(self.turn_type);
    }
    let stop_reason = self.pending_approval_stop.take().or(stop_reason);
    self.emit_done(stop_reason)?;

    // Post-turn memory auto-extraction (best-effort, fire-and-forget).
    if let Some(hook) = self.memory_extraction_ctx() {
        tokio::spawn(async move { memory_extraction::extract_and_store(hook).await });
    }

    return Ok(());
}
```

`memory_extraction_ctx()` builds an owned `ExtractionCtx` (project root, the
user message text, a weak-tier provider/model handle, the memory dir) only when
extraction is enabled and a user message exists for this run. Spawning
detaches it from the run lifecycle: a slow or failed extraction cannot delay the
host. Gate behind a config flag (default on; opt out for tests/headless).

### 4.2 The extraction module

New `craft-agent/src/agent/memory_extraction.rs`:

1. `worth_extracting(text) -> bool`: keyword pre-filter. Port clark's cue list
   (~30 phrases) and adapt to craft's tone. Keep it cheap and conservative —
   false negatives are acceptable (we just skip a save), false positives waste a
   cheap model call. Unit-test with `#[test_case]`.
2. `ExtractionCtx`: owned struct — `project_root: PathBuf`, `memory_dir: PathBuf`,
   `user_text: String`, `model: Model` (weak tier), `existing_titles: Vec<String>`.
3. `extract_and_store(ctx) -> Result<()>`:
   - If `!worth_extracting(&ctx.user_text)`, return early.
   - Load existing memory titles from `ctx.memory_dir` (reuse the listing logic
     from `memory_helpers`, exposed as a Rust helper or reimplemented minimally).
   - Build a strict JSON-only system prompt: extract up to `MAX_FACTS = 4` facts,
     each `{title, content, scope, type, source, supersedes}`. Pass
     `existing_titles` and instruct: mark `supersedes` with an existing title
     when the new fact replaces it; empty string otherwise; return
     `{"facts":[]}` when nothing durable was stated.
   - Call `ctx.model` via `Provider::stream_message` and collect the text.
   - `parse_extraction(reply) -> Option<Extraction>`: tolerant JSON extraction
     (first `{` to last `}`), strip code fences. Port clark's `parse_extraction`
     (`memory_extraction.rs:249-253`).
   - For each fact: validate non-empty title/content; write to the scope's
     memory dir as markdown; if `supersedes` is non-empty, delete the old note
     (trying project then global scope).
   - Tag provenance in the written content (`source: user-stated | inferred`).
4. All errors are logged (structured, `tracing`) and swallowed. Never propagate.

### 4.3 Storage interop with the Lua memory plugin

Craft's memory is owned by the `memory` Lua plugin (`plugins/memory/init.lua`).
Two integration options:

- **Preferred: shared markdown layout.** Write extracted facts as markdown files
  in the same `<state>/projects/<id>/memories` directory the plugin reads, using
  the same filename/content conventions the `memory` tool's `write` command
  produces. Then the plugin's `after_instructions` prompt hint
  (`init.lua:29-62`) lists them automatically, and the `memory` tool can
  view/update/delete them. This requires a Rust-side writer that matches the
  plugin's on-disk format — expose the format via `memory_helpers` or a small
  shared spec.
- **Alternative: route through the plugin.** Call the Lua `memory` tool's write
  path from Rust. Heavier and couples Rust to Lua; avoid unless the format
  cannot be matched directly.

Either way, the extracted notes must be visible to the existing prompt hint and
editable by the existing `memory` tool — extraction is a *writer* into the
existing memory store, not a parallel store.

### 4.4 The model role

Add a `memory_extractor` role to `model_roles.toml` at the weak tier
(e.g. haiku-class). Extraction is a side-completion; it should be cheap and
fast, never the user's configured main model unless they want it to be. Make
the role overridable. If no model is configured for the role, fall back to the
run's weak-tier default; if no provider is available, skip extraction silently.

### 4.5 What extraction must NOT do

- Never block the run, the host re-prompt, or `emit_done`.
- Never surface errors to the user (log only).
- Never extract from assistant text — only from the user's message for this run
  (clark keys off `user_text`). Avoids amplifying model hallucinations.
- Never extract secrets, credentials, or full file contents. The system prompt
  must instruct extraction of *durable facts the user stated*, quoting the
  user's words rather than paraphrasing (clark convention).

## 5. Acceptance criteria

- [ ] After a run whose user message contains a durable fact (e.g. "we're
  rebranding to X"), a memory note appears in
  `<state>/projects/<id>/memories` without the model calling the `memory` tool.
- [ ] The note is listed by the existing `after_instructions` prompt hint and is
  viewable/editable via the existing `memory` tool.
- [ ] Supersession works: a second run stating an updated fact with the prior
  title replaces (deletes the old note, writes the new).
- [ ] A run whose user message is pure task traffic ("fix the failing test")
  does not spend a model call (pre-filter returns false).
- [ ] Extraction failure (provider error, parse error) is logged and does not
  break the run or surface to the user.
- [ ] Extraction is gated by a config flag and off by default in tests.
- [ ] `cargo nextest run --all-features --workspace` and
  `cargo clippy --all-features --all --tests -- -D warnings` pass.

## 6. Tests

- Unit: `worth_extracting` true/false cases (`#[test_case]`).
- Unit: `parse_extraction` handles plain JSON, code-fenced JSON, and prose-wrapped
  JSON; returns `None` on no-JSON.
- Integration: a scripted provider (`craft-providers/src/providers/synthetic.rs`)
  returns a fixed extraction JSON; assert the note is written, superseded note
  deleted, and the run completes normally when extraction is detached.
- Integration: extraction error path — synthetic provider returns an error;
  assert the run still completes and `emit_done` fired.

## 7. Risks and notes

- **Latency/cost.** One weak-tier side-completion per qualifying turn. The
  pre-filter keeps it rare. Make the tier and the on/off flag configurable.
- **Duplication.** Without supersession, repeated facts accumulate. Supersession
  (passing existing titles) is the mitigation; mirror clark's design.
- **Privacy.** Extraction sends the user's message text to the model. This is
  the same trust boundary as the main run (the message already goes to the
  provider), but document it. Extraction must never send raw file contents or
  secrets — only the user's stated message, and only durable facts.
- **Flow mode.** In Flow mode, each stage is a short-lived subagent. Extraction
  should run at the *workstream* level (on the root/general turn), not after
  every narrow stage turn, to avoid noise. Gate by `AgentMode` if needed.
- **Why not just prompt harder?** clark's own evidence: in-loop `memory` tool use
  is unreliable regardless of prompting. The structural post-turn fix is the
  point.
