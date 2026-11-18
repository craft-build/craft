# The Harness

One long-lived `harness::Conversation` per session drives an animist `Agent` over an animist `History`; surfaces never run a loop of their own. This page describes what runs under the TUI, `--print`, and ACP alike.

## Runtime setup contract

Every execution surface obtains the shared pieces of a session from `agent/src/runtime.rs`, so equivalent sessions receive equivalent run policies:

- `runtime::workspace_env(cwd, mcp)` — instruction discovery, the workspace (MCP handle installed), and the permission engine. Every surface constructs the `PermissionManager` and consults it: the TUI prompt overlay, the ACP `session/request_permission`, and the headless permission gate.
- `model_selection::resolve_or_default` — a model choice plus its catalog window; output ceilings ride the model handle, where request shaping reads them. Lookup only: `--model` ids the catalog does not list stay usable.

## The layer chain

Craft policy lives in an ordered `AgentLayer` chain (head → tail):

1. **PolicyLayer** — permission rules → auto-review → the approval responder → the plan-mode gate.
2. **GuardrailLayer** — doom-loop detection (grace/hard-stop), per-tool guardrails, dedup `Respond`.
3. **RequestLayer** — the per-request view: in-run read-lifecycle and compression rewrites, Chat-Completions image relocation, the output-token clamp, sampling options, fallback-model selection.
4. **RecoveryLayer** — `recover`: linear backoff, API-key rotation, model fallback chain, reauth wait, overflow compaction.
5. **ContinuationLayer** — `MaxTokens` continuation and stalled-turn nudges as `step_end` appends.
6. **SnapshotLayer** — `/undo` pre-captures at the tail, so only calls that really run are captured.
7. **PersistLayer** — flushes the history change feed to the session log.

## The driver

A thin driver (`harness::driver::run_turn`) does what no hook can: builds the input, runs and awaits the agent, adds terminal markers, runs the between-run alteration/compaction passes, and re-runs for the advisor and `ContextWindowExceeded`.

Model switching happens between runs by rebuilding the agent on the same history handle; fallback models inside a run go through the request layer.

## Subagents

The `task` tool spawns child agents as their own `Conversation` with:

- a fresh history — delegation never pollutes the main session;
- a restricted tool registry;
- their own layer instances built from the parent's shared services;
- model tiers (`weak` / `medium` / `strong`, capped at the parent's tier);
- cancel tokens, optional worktree isolation, and optional JSON-schema-validated output.

`batch` fans children out concurrently under the parent's claims and budgets.

In the TUI, each subagent gets its own chat window (Ctrl-N / Ctrl-P; `/tasks` view). See [Tools](./tools.md) for the tool surface and [TUI](./tui.md) for the windows.

## Advisor

An optional post-turn reviewer (`agent.advisor`, off by default) reads each reply and emits at most one note. `auto_act` (`off` / `nit` / `concern` / `blocker`) sets the minimum severity that triggers an automatic follow-up turn instead of stopping, bounded by `max_act_turns`.
