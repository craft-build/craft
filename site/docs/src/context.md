# Context Management

Craft reduces the conversation content sent to the model as you work. It compresses long tool results, replaces outdated file reads with markers, and compacts older history into summaries. The original content remains in the saved history and can be recovered with the `retrieve` tool.

You usually do not need to manage this yourself. Use `/compact` in the TUI to compact immediately, or adjust the [compression and compaction settings](./configuration.md#compaction-stages).

## Tool-output compression

Between runs, an alteration pass classifies every oversized tool result by content type — code, log, diff, JSON array, search result, plain text — and rewrites it into a compact marker as a history *revision*. Log and code compression use an aho-corasick keyword classifier (error, warning, security, definition, import, ...) so the lines that matter survive.

The verbatim originals stay in the entry sources, and the `retrieve` tool restores them by content hash:

```
[137 lines compressed from 892. Retrieve original: hash=a91f33c2]
```

Nothing lives in a side store: a marker survives session reloads, and retrieval works in any later turn. Within a run, the request layer recomputes only the run's own suffix (everything older arrives pre-altered).

Tune rates, line limits, per-file match limits, and `protect_recent_tool_outputs` in the `compression` block. The last setting leaves the most recent N tool outputs unchanged. See [Configuration](./configuration.md#tool-output-compression).

## Read lifecycle

File reads are tracked fresh → stale → superseded. As the conversation moves on:

- **stale** reads get durable marker revisions, so old snapshots stop occupying context;
- **superseded** reads (the file was edited since) collapse the same way;
- `retrieve` still serves the original bytes from the entry source.

## Two-stage compaction

Named `compaction` blocks select a strategy and a `context` fill ratio. For example, `"0.6"` means 60% of the context window. Between runs, Craft estimates history size and runs eligible stages whose thresholds have been crossed, lowest ratio first:

```bml
compaction "vcc" {
  context = "0.6"
}
compaction "llm" {
  context = "0.8"
}
```

- **`vcc`** — deterministic, no-LLM compaction: the head of the history is replaced by a structured summary entry (tagged `craft.compaction`); the recent tail is kept verbatim; repeated runs merge with the previous summary.
- **`llm`** — one-shot summarization call replacing the head with a handoff summary (deterministic fallback if the call fails).

Omitting the blocks defaults to vcc at 0.6 then llm at 0.8.

**Effectiveness gating** skips a stage whose last run saved under 10% of estimated context until another stage compacts effectively, so compaction never cycles. Sessions without a known `context_length` never compact. Compacted-away entries are *deactivated*, never deleted — their originals stay in entry sources for `retrieve`.

`compaction_buffer` (absolute tokens ≥ 1000, or a quoted percent like `"20%"`) reserves headroom so compaction fires before the window fills.

## Overflow recovery

When a request still overflows after compaction, an escalating ladder collapses tool results progressively — 10%, 20%, 50%, 100% — before failing the turn. A failed run keeps whatever compaction it did; originals are never lost.

## Dedup cache

Identical read-only tool invocations return cached output; entries are invalidated when files change. This cache is part of the [guardrail layer](./harness.md).

## Compaction on demand

`/compact` in the TUI runs the compaction stages immediately. Compaction summaries render as cards in the transcript (see [Sessions](./sessions.md)).
