# Feature spec: Dynamic primacy/recency prompt slotting

Status: implementation specification, ready to implement (opportunistic —
highest value once per-turn fresh context lands). Part of the clark-code feature
review (see `docs/clark-code-review-assessment.md`).

## 0. Summary

LLM attention has a primacy and recency bias: it attends strongly to the start
of the context (the system prompt's hard rules) and to the end (the latest
turn). clark-code exploits this by keeping hard rules in the primacy slot
(directly after the identity line) for prompt-cache stability, and injecting
volatile per-turn facts (a fresh `git status` snapshot, current output style)
at the *recency* end of each turn message — not in the system prompt. This keeps
the cached system-prompt prefix stable across turns while putting fresh facts
where recency bias helps most.

Craft already has a static `Slot` system with a stable cacheable prefix and
Anthropic prompt caching wired at the provider layer. What it lacks is the
*per-turn volatile facts in the recency position of the turn message* (a place
for fresh, discardable context that should not pollute the cached system prefix
or accumulate in long-term history). This spec adds a `Recency` turn-context
channel: a small, per-turn bundle of volatile facts attached to the latest user
turn, rebuilt fresh each turn and never persisted.

This is the lowest-priority of the four specs because craft's caching already
works; it becomes valuable when craft adds per-turn fresh context (live repo
state, current TODO state, recent error context).

## 1. Why this matters

Two failure modes today:

- **Volatile facts in the system prompt break caching.** If a fresh `git status`
  snapshot is injected into the system prompt, the cached prefix changes every
  turn and caching stops helping. Keeping it out of the system prompt preserves
  the cache.
- **Volatile facts in long-term history accumulate noise.** If the snapshot is
  appended as a user message, it stays in history forever (until compaction),
  wasting tokens and confusing later turns. A recency channel is per-turn only:
  present now, gone next turn.

The recency position (end of the latest turn) is also where attention is
highest, so time-sensitive facts ("the test you just ran failed with X") land
where the model is most likely to act on them.

## 2. Reference: how clark-code does it

clark `crates/provider-local/src/prompt.rs:1-307` assembles the system prompt
with hard rules in the primacy slot directly after the identity line, and
deliberately keeps volatile per-turn facts out of it. Instead, volatile facts
(a fresh `git status` snapshot, output style) are injected at the recency end of
each turn message. The cached system-prompt prefix is identical across turns,
so prompt caching stays effective; the per-turn volatile block is rebuilt every
turn and never persisted to the transcript. This pairs with their
autoregressive-schema-ordering work: both treat prompt *position* as part of
the prompt.

The principle is general (primacy/recency from the prompt-engineering
literature); the load-bearing detail is *separating the stable system prefix
from the volatile per-turn tail* so caching and recency both win.

## 3. Where this lives in craft

| Concern | Location |
|---|---|
| Existing slot system | `craft-agent/src/prompt.rs:64-100` — `SlotKind` (`Singleton`/`Aggregate`), `Slot` (`Identity, Tone, ToolUsage, EfficientTools, SubagentBriefing, Conventions, AfterInstructions`), `marker()` (`:82-91`), `assemble` (`:208-214`), `assemble_raw` (`:216-225`) |
| System prompt templates | `craft-agent/src/prompts/*.md` (`{{identity}}`, `{{after_instructions}}`, etc.) |
| Plugin prompt hints | `craft.api.register_prompt_hint` (Lua) targeting a `Slot`; e.g. `plugins/memory/init.lua:29-67` |
| Anthropic prompt caching | `craft-providers/src/providers/anthropic/shared.rs:209-239` (`cache_control: ephemeral` breakpoints; `MESSAGE_CACHE_BREAKPOINTS`) |
| Turn message construction | `craft-agent/src/agent/run.rs` turn loop; provider request assembly in `craft-providers/src/providers/*` |
| New: recency channel | `craft-agent/src/prompt.rs` (a new `RecencyFacts` type + builder) and the turn/request assembly path |

## 4. Implementation

### 4.1 Do not add a new system-prompt slot

The key constraint: the system prompt must stay stable for caching. So the
recency channel is **not** a new `Slot` in the system prompt. Adding volatile
content to `Slot::AfterInstructions` would change the cached prefix every turn.
The recency channel is a separate, per-turn artifact attached to the latest
user turn at request-build time.

### 4.2 The `RecencyFacts` channel

Add to `craft-agent/src/prompt.rs`:

```rust
pub struct RecencyFacts {
    blocks: Vec<String>,  // ordered; rendered as a fenced/sectioned tail
}

impl RecencyFacts {
    pub fn new() -> Self { Self { blocks: Vec::new() } }
    pub fn push(&mut self, block: String) { self.blocks.push(block); }
    pub fn is_empty(&self) -> bool { self.blocks.is_empty() }
    pub fn render(&self) -> String { /* join blocks under a "<turn-context>" header */ }
}
```

It is rebuilt fresh every turn and never written to `History`. It is a
request-time decoration of the latest user message, not a persisted message.

### 4.3 Populate it per turn

In the turn loop (`craft-agent/src/agent/run.rs`), build a `RecencyFacts` before
each provider request. Sources are opt-in and pluggable, mirroring how `Slot`
hints are registered:

- A `register_recency_source` Lua API (`craft.api.register_recency_source`)
  yielding a function `(ctx) -> string | nil`, evaluated per turn. This is the
  extension point plugins use.
- Built-in sources added as needed, e.g.:
  - A fresh `git status` snapshot for the current repo (cheap, time-sensitive).
  - The current TODO list state (from the `todo_write` tool's live store).
  - Recent error context (last failed tool result, if relevant).
  - Current output style / mode, if volatile.

Keep the default set small. Every source must be cheap (sub-millisecond to a
few ms) since it runs every turn on the latency path.

### 4.4 Attach at request-build time

When assembling the provider request, append the rendered `RecencyFacts` to the
*latest* user message only — not to every message, and not into the system
prompt. Concretely, the request builder takes the assembled history and, for the
final user turn, produces a synthetic variant with the recency block appended.
Because the recency block is not persisted to `History`, the next turn sees a
clean user message and rebuilds its own recency block.

For Anthropic specifically, ensure the cache breakpoint placement
(`craft-providers/src/providers/anthropic/shared.rs:209-239`) still breaks
*before* the volatile tail so the tail is not cached. The stable prefix (system
prompt + prior turns) remains the cached region; the recency tail of the final
turn is the uncached suffix. This is the mechanism that lets caching and
recency coexist.

### 4.5 Backward compatibility

- Default `RecencyFacts` is empty. With no sources registered, the request is
  byte-identical to today. No behavior change for existing users or tests.
- Existing `Slot` hints and the system prompt are untouched.
- Tests that pin system-prompt output continue to pass (the system prompt is
  unchanged).

## 5. Acceptance criteria

- [ ] A `RecencyFacts` channel exists, is rebuilt per turn, and is never written
  to `History`.
- [ ] With no recency sources registered, provider requests are byte-identical
  to today (regression test).
- [ ] When a source is registered (e.g. a test fixture injecting a marker
  string), the rendered block appears at the end of the latest user turn in the
  provider request, and **not** in the system prompt, and **not** in any prior
  turn.
- [ ] The Anthropic cache breakpoint still precedes the volatile tail; the
  cached prefix is stable across turns when only the recency tail changes
  (assert via a test that builds two consecutive requests with different
  recency tails and confirms the cached prefix bytes are identical).
- [ ] `cargo nextest run --all-features --workspace` and
  `cargo clippy --all-features --all --tests -- -D warnings` pass.

## 6. Tests

- Unit: `RecencyFacts::render` formatting; empty case.
- Unit (regression): no sources → request body equals today's request body.
- Integration: register a recency source via the Lua API; assert the block is in
  the final user turn of the assembled request and absent from the system prompt
  and from `History` after the turn.
- Integration (caching): two requests differing only in the recency tail share
  an identical cached system+history prefix.

## 7. Risks and notes

- **Latency.** Every source runs every turn. Keep the default set empty or
  near-empty; document that sources must be cheap. Profile before adding a
  source that hits the filesystem or runs a subprocess.
- **Token cost.** The recency tail is sent every turn and is not cached. Keep
  it small (a few hundred tokens). Bound it; drop/truncate oldest blocks if a
  budget is exceeded.
- **Leaking into history.** The cardinal rule: recency facts must not be
  persisted. If they leak into `History`, they defeat the purpose (accumulate
  forever). Test this explicitly.
- **Other providers.** OpenAI/Google/etc. do not have explicit cache-control
  breakpoints, but the principle (stable prefix, volatile tail) still helps
  their implicit prefix caching. The mechanism is provider-agnostic; only the
  breakpoint placement is Anthropic-specific.
- **Why this is lower priority.** Craft's static slots + existing Anthropic
  caching already deliver most of the value. This spec earns its keep when
  craft adds genuinely per-turn volatile context. Land it alongside the first
  such source (e.g. live repo state), not in isolation.
