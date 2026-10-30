# Rules for agents working in `agent/`

These apply to every change in this crate. During the animist migration, also
follow [`docs/animist/harness-design.md`](../docs/animist/harness-design.md),
which wins where the two disagree.

## animist is the substrate

- Use animist types directly: `Provider`, `Model`, `ModelInfo`, `Catalog`,
  `History`, `Message`, `Agent`, layers, `ToolImpl`. Don't define a craft
  struct that mirrors an animist one, and don't reuse an animist type name
  for a different craft type.
- A craft type around an animist type has to carry data animist can't. A
  struct that projects or renames fields to avoid updating call sites is not
  allowed: update the call sites.
- Don't write pass-through functions (`fn model(..) { catalog.model(..) }`).
  Call the animist API.
- **Missing upstream is not a reason to drop behaviour.** If craft did
  something before and animist doesn't provide it, choose one:
  - it is application policy (caching, config, UI, pricing schedules), so
    craft implements it, small and directly on animist's API;
  - it belongs in animist, so keep the craft behaviour working and write a
    followup in `~/Projects/animist/docs/followups/`.

  Never remove it with a comment saying it "belongs upstream".
- Craft never edits animist, and depends on the crates.io release.

## No hidden state, no middlemen

- No process-global mutable state (`static` + `OnceLock`/`RwLock`/`Mutex`
  holding runtime data). Build things once at startup or in
  `ConversationBuilder`, and pass them explicitly (`Arc<T>` where they're
  shared). Constants and lazily compiled regexes are fine.
- Before adding a trait, wrapper, `Shared*` alias, channel hop or params
  struct, name the second implementation or the caller it decouples. If you
  can't, use the concrete thing.
- Policy lives in layers and runs once for every surface. Surfaces (TUI, ACP,
  headless, print) render and collect input. They don't make policy,
  compaction or retry decisions.

## Async

- No `block_on`, `block_in_place` or private runtimes. `spawn_blocking` only
  for CPU-bound or sync-only libraries, with a one-line reason comment.
- Never hold a `std::sync` lock across `.await`. No detached tasks for work
  whose result or cleanup matters. Await it.
- A function returns `Result` only if it can fail.

## Errors and style

- **snafu, used properly.** No `anyhow`/`thiserror`, and no hand-built
  variants.
  - Wrap a fallible call with `ResultExt::context(Selector { .. })`. Use
    `with_context(|_| Selector { .. })` when building the fields costs
    something.
  - Turn `None` into an error with `OptionExt::context(Selector)`.
  - Raise an error with `Selector { .. }.fail()` or `ensure!(cond, Selector
    { .. })`, not `return Err(Error::Variant { .. })`.
  - Each variant names one failure, carries the fields that explain it
    (path, provider, model) and keeps its `source`. Box recursive sources
    with `#[snafu(source(from(Error, Box::new)))]`.
  - Never flatten an error with `.to_string()` / `map_err` into a string
    field, and don't route new failures through the catch-all `Invalid {
    reason }` or `ProviderClient`. Add a variant instead.
  - Display text is a short lowercase phrase saying what was being done
    (`"reading {path}"`), without the source. The report renders the chain.
  - Errors from another crate (animist, rmcp, argosy) stay as the `source`
    of a craft variant. They aren't converted to text or re-wrapped
    generically.
- **No narrative comments.** A comment states what the code does or the
  constraint it upholds, briefly. It doesn't explain the migration, history,
  alternatives considered, or why a design is right. That belongs in the
  completion note or the commit message. Delete a comment that only restates
  the code.

## Behaviour and tests

- Preserve behaviour unless the task names the change. Pin every intentional
  change with a test and list it in the completion note.
- Don't delete, weaken or `#[ignore]` a test to get green. An ignore needs a
  reason naming what restores it. A removed test needs a named replacement.
- Tests don't share global state: build fixtures locally, so tests stay
  parallel-safe.
- No paid model calls. Use the scripted provider, local mock servers and
  temp dirs. Never write credentials to files.
- Validate with `cargo fmt -p craft --check`,
  `cargo clippy -p craft --all-targets` and `cargo test -p craft`.
