# Flow-mode base prompt: port one component from reference craft

You are implementing **{{PHASE}} / task #{{TASK}}** from `order.md` in this repo
(`/Users/akarifur/Projects/craft-code`).

## Setup (do this first, every time)

1. Read `order.md` and find the exact task under the given phase (e.g. "Phase 1 — task 12:
   D.1 Token estimation & calibration").
2. Read the matching entry in `comparison.md` — it gives the **What/why**, **Ref** (where the
   feature lives in the reference), **Effort**, and **Depends on**. Honor the dependencies;
   if a listed dependency is not yet ported, stop and report instead of improvising.
3. The reference implementation lives at `~/Projects/craft` (v0.14.1, ~16 crates). The
   **Ref** field in `comparison.md` names the crate + path. **Always read the reference
   code before writing anything** — port the logic, not a from-scratch reimplementation.
   Do not line-by-line copy; adapt to this repo's Rig-based architecture.
4. Read this repo's existing code where the feature will land (`agent/src/...`) and match
   its style, error handling, and patterns.

## Ground rules

- **Architecture difference**: the reference owns its run loop; this repo delegates to Rig.
  Loop-resident reference logic must be re-expressed as Rig hooks/runner wrappers unless
  the comparison entry says otherwise.
- Only implement the one task given. Note (but do not implement) anything adjacent you
  discover.
- Every ported behavior needs tests, in the same style as the existing suites
  (`agent/src/tools/tests.rs`, `agent/src/tui/` mock-provider tests).
- Do not break existing tests; run `cargo test` and `cargo clippy` before finishing.
- Never commit unless explicitly asked.

## Flow-mode instructions

- Goal: task {{TASK}} of Phase {{PHASE}} from `order.md` is implemented, tested, and
  passing in this repo.
- Work stage by stage through the pipeline; the verifier stage must confirm tests pass
  and the behavior matches the reference's semantics (not just its shape).
- If the reference code contradicts `comparison.md`, trust the code and say so in the
  report.
