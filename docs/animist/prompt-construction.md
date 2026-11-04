# Migrate prompt construction to animist's `Prompt` / `Section`

## Required workflow

- Load the `rust-best-practices` skill before writing any code.
- Follow [`harness-design.md`](harness-design.md), especially
  [Test integrity](harness-design.md#test-integrity) and
  [State ownership](harness-design.md#state-ownership). Save the test inventory
  before the first edit and diff it at the end.
- Read `agent/AGENTS.md`. animist is the substrate: no craft type may mirror
  `Prompt` or `Section`, and no pass-through wrapper earns its place here.
- Finish with the review agent. Tell it to check the diff for weakened
  assertions and for any second prompt-construction path left behind.

## Goal

Craft builds every system prompt with its own template engine in
`agent/src/prompt.rs`: `Slot`, `SlotKind`, `PromptId`, `ResolvedSlots`,
`assemble`, `assemble_raw`, `fill_marker`, `Vars`, and the `{{marker}}`
templates under `agent/src/prompts/`. animist ships the same capability as a
typed model — `animist::agent::{Prompt, Section}` — and craft never uses it.

Replace the template engine with animist's types:

- `prompt::system`, `prompt::research`, `prompt::general` return
  `animist::agent::Prompt`, built from `Section`s.
- The model sees the rendered `Prompt` in the format the user selects via
  `agent.prompt_format` (`"markdown"` or `"xml"`). Both renderers are
  animist's — builders produce a `Prompt`, one dispatch point renders it, so
  there is still exactly one code path.
- Section identity (`Section::name`) becomes the addressing key for overrides,
  tests and future plugin runtimes — a stable contract that string splicing
  could never provide.
- The hand-rolled engine is deleted, not wrapped.

## Non-goals

- Do not change tool schemas, compaction behaviour, or the provider layer.
- Do not touch `agent/src/recipe.rs`. Recipe bodies are user templates and
  keep minijinja. Note for the record: the current system-prompt path is *not*
  minijinja — it is `str::replace` over `{{marker}}` in `fill_marker`
  (`agent/src/prompt.rs:312`).
- Do not move prompt assembly into `Agent::builder().prompt(..)` yet; see
  [Decisions](#decisions).
- No CLI flag or TUI/runtime switch for the render format; config only.
- User-message templates (recipes, compaction user templates) never render
  through `PromptFormat`; the option covers system prompts only.

## Read first

- animist 0.7.0: `src/agent/prompt.rs` (`Section`, `Prompt`, `to_markdown`,
  `to_xml`), `src/agent/mod.rs:172` (the agent renders `prompt.to_markdown()`
  into `StepRequest::system` before layers run), `src/agent/run.rs` →
  `StepRequest`, and `docs/agents.md` → *Construction and lifecycle*,
  *Execution, history, and cancellation*.
- Craft:
  - `agent/src/prompt.rs` — everything above, plus `today_utc`,
    `civil_from_days`, and `build_system_prompt`.
  - `agent/src/prompts/{system,research,general,plan}.md` — the content being
    mapped.
  - `agent/src/harness/request.rs:109` (`RequestLayer::system_prompt`),
    `:124` (`turn_start`), `:168` (`request.system = system`),
    `:269` (`system_prompt_bytes_match_the_old_policy_output`).
  - `agent/src/harness/conversation.rs:455` (`preamble_base`) and `:516`
    (`build_agent`).
  - `agent/src/subcmd.rs:385` (`craft prompt …`).
  - `agent/src/subagent.rs:97` (role one-liners), `:333` (child preamble).
  - `agent/src/compaction/llm.rs:18` and `agent/src/auto_review.rs:22`
    (single-shot system prompts).
  - `docs/history/animist/05-agent-runtime-core.md:181` — the clause that
    allowed today's `StepRequest::system` shortcut; this task supersedes it.

## Current state

`build_system_prompt` (`agent/src/prompt.rs:382`) prepends an environment
block to `config.agent.preamble + instructions`, substitutes that into the
`{{instructions}}` marker of `system.md`, then appends the plan-mode markdown
when a plan path is set. `RequestLayer` calls it at `turn_start`, stores the
resulting `String`, and writes it into `StepRequest::system` in
`prepare_request`. Subagent children get a longer `config.agent.preamble`
(parent base + role one-liner) and run the same template. `craft prompt`
reuses the same functions for display.

The slot machinery is already caller-less: `ResolvedSlots::default()` is the
only construction at every production site (`harness/request.rs:116`,
`subcmd.rs:447,453,459`, `harness/tests/spec.rs:89`). `Slot::from_str`,
`PromptId::from_str`, `has_slot` and the singleton/aggregate kinds have no
non-test caller. They are a plugin contract with no plugin runtime behind it.

## The ABI being adopted

```rust
pub struct Section {
    pub name: String,      // stable id; also the markdown heading and XML attribute
    pub title: String,     // human heading text
    pub summary: String,   // one prose block, followed by the rendered rules
    pub rules: Vec<String>,
}
```

`Section::to_markdown()` is exactly:

```text
# {name} - {title}
{summary}
- {rule}
- {rule}
```

`Prompt::to_markdown()` joins sections with a single `\n`. There is no blank
line between sections, and no trailing newline. Every section always renders a
heading, even with an empty `summary` (which leaves an empty line).

`Section::to_xml()` renders `<section name="...">` with `<title>`,
`<summary>` and `<rules>`/`<rule>` children; all text is XML-escaped.
`Prompt::to_xml()` joins the section elements with a single `\n`. Both
renderers are the complete ABI surface craft consumes — there is no
craft-side post-processing in either format.

Constructor note: both types are `#[derive(Struct!, Builder)]` with
`#[builder(on(_, into))]`, so `Section::builder().name("…").title("…")
.summary("…").rules(vec![…]).build()` accepts `&str` and `Vec<String>`. They
are `Debug + Clone + Default` only — not `Serialize`, not `PartialEq`. Do not
put them in config or session files; compare rendered strings in tests.

## Mapping

`name` is the visible heading and the stable identifier. `title` is a short
lowercase clarifier, because `to_markdown` prints both. `summary` holds today's
heading-less prose; `rules` holds today's bullets **without** the `- ` prefix,
which `Section` adds.

### `system.md`

| Today | Section (`name` / `title`) | Content |
| --- | --- | --- |
| `{{identity}}` | `Identity` / `who you are` | `summary` = `DEFAULT_IDENTITY` |
| `# Tone and style` + `{{tone}}` | `Tone and style` / `how to write` | `rules` = `DEFAULT_TONE` lines, prefixes stripped |
| `# Professional objectivity` | `Professional objectivity` / `accuracy over agreement` | `summary` = both paragraphs |
| `# Tool usage` bullets + `{{tool_usage}}` | `Tool usage` / `how to use the tools` | `rules` = the five static lines, then the efficient-tools line, then plugin extras |
| `{{efficient_tools}}` | folded into `Tool usage` | `Most efficient tools: {names}.` as a rule |
| `{{subagent_briefing}}` | `Delegating to subagents` / `brief like a colleague` | `summary` = the intro sentence, `rules` = the five bullets |
| `# Conventions` + `{{conventions}}` | `Conventions` / `rules that always apply` | `rules` = the six defaults, then extras |
| `# When done` | `When done` / `finish cleanly` | `rules` = the one bullet |
| environment block | `Environment` / `where you are running` | `rules` = `Working directory: …`, `Platform: …`, `Date: …` |
| `{{instructions}}` | `Instructions` / `user and project instructions` | `summary` = `config.agent.preamble` + discovered instructions, verbatim |
| `{{after_instructions}}` | plugin `Section`s after `Instructions` | omitted entirely when there are none |
| `plan.md` | `Plan mode` / `read-only until approved` (plan mode only) | `summary` = the CRITICAL paragraph and responsibility sentence, `rules` = one per planning obligation, the last naming the plan path |

Section order is the table order: `Identity`, `Tone and style`,
`Professional objectivity`, `Tool usage`, `Delegating to subagents`,
`Conventions`, `When done`, `Environment`, `Instructions`, plugin sections,
`Plan mode`.

### `research.md`

`Research agent` / `read-only investigation` (summary = both opening
paragraphs) → `Environment` → `Output discipline` (summary = the
injected-context paragraph) → `Output format` (rules = the four return shapes,
the NEVER lines and the URL line) → `Exploration budget`
(rules = the three budgets plus the never-inflate line) →
`Verify before recommend` (summary) → `Tool usage` (rules) → `Guidelines`
(rules) → `Instructions`.

### `general.md`

`General-purpose agent` (summary = the opening paragraph) → `Environment` →
`Output discipline` (summary = the injected-context paragraph, rules = the
five lines and the URL line) → `Tool usage` (rules) → `Conventions` (rules) →
`When done` (rules) → `Instructions`.

### Subagents

`RESEARCH_PROMPT` and `GENERAL_PROMPT` (`agent/src/subagent.rs:97`) become one
`Subagent role` / `what you are` section appended after the parent's
`Instructions` section, replacing the `format!("{}\n\n{}", base, role)` join
at `agent/src/subagent.rs:333`. The reviewer role
(`agent/src/tools/review.rs:50`) follows the same shape.

### Slug state that must survive

- The `Environment` values: `cwd`, `std::env::consts::OS`, `today_utc()`.
  Format them in Rust; `Vars` and `{cwd}`-style substitution go away.
- `NATIVE_EFFICIENT_TOOLS` plus plugin-supplied names, rendered as the
  `Most efficient tools: …` rule. The list must only name real tools.
- `config.agent.preamble` (already resolved by `Cli::apply_prompt_overrides`
  exactly once) and the discovered instructions, verbatim and exactly once.
- Plan mode: the plan path reaches the model, and build mode contains no plan
  section at all.
- `craft prompt system|research|general --plan --tools --names` behaviour.
- `config.agent.prompt_format`, read once at conversation build, deciding the
  rendered bytes of every system prompt in the session.

## Decisions

1. **The layer renders; the agent keeps its default prompt.** `Agent::prompt`
   is conversation-scoped, but plan mode, prompt overrides and the date change
   per run without rebuilding the agent
   ([harness-design.md → State ownership](harness-design.md#state-ownership)).
   `RequestLayer` therefore keeps the conversation-scoped pieces (preamble +
   instructions, cwd, extra sections) and builds the `Prompt` at `turn_start`
   — the date in the `Environment` section is run-scoped, as it always was —
   appends `Plan mode` when the settings ask for it, and sets `request.system`
   to the rendered bytes.

   *Rejected:* putting the base `Prompt` on `Agent::builder().prompt(..)` and
   rebuilding the agent on a mode switch — churn on the model/tool rebuild
   path, and the layer would still need the base to append the plan section.
   Revisit if animist exposes run-scoped prompt overrides.

   *Rejected:* a craft renderer over `Prompt` to reproduce today's bytes — that
   is the duplicate capability this task removes.

2. **Accept the ABI's bytes.** The rendered prompt changes shape: every section
   gains `# {name} - {title}`, sections are separated by one newline instead of
   a blank line, `plan.md`'s `<system-reminder>` wrapper and `##` sub-headings
   become a `Plan mode` section, and a subagent's role line becomes its own
   `Subagent role` section instead of riding inside `Instructions`. Pin the new
   goldens and list this as a behaviour change. Do not post-process
   `to_markdown`. "The rendered prompt" now has two accepted byte shapes — one
   per format: both goldens get pinned, and neither format is post-processed.

3. **Delete the slot engine, do not port it.** No production caller exists
   (see [Current state](#current-state)). If a plugin runtime lands later it
   supplies `Section`s, and extending a built-in section means merging by
   `Section::name` at that point. Do not add an overrides map, `FromStr`
   parsing or a `Shared*` handle now.

4. **One construction path for every system prompt.** Chat agents, subagent
   roles, `craft prompt` display, the auto-review reviewer and the compaction
   summarizer all build `Prompt`s. Compaction's two *user* templates stay
   strings: they are user messages, not system prompts.

5. **The render format is a config choice dispatched at one place.**
   `AgentConfig.prompt_format: PromptFormat` (default `Markdown`) in
   `agent/src/config.rs`, deserialized from `"markdown"`/`"xml"` (unknown
   values rejected by `deny_unknown_fields`-style validation, same pattern as
   `CompactionKind`). `PromptFormat::render(&animist::agent::Prompt) -> String`
   is a single `match` on the enum calling `to_markdown()`/`to_xml()` — enum
   and method live in `agent/src/prompt.rs` (application policy over animist's
   API, permitted by `agent/AGENTS.md`); it is not a wrapper that mirrors an
   animist type. `ConversationBuilder` copies the value into
   `RequestLayerInputs`; `prepare_request` keeps its single assignment as
   `request.system = inputs.prompt_format.render(&prompt)`. Perf note: the
   layer renders per run as today (plan section appended at `turn_start`), so
   the format costs nothing per step.

   *Rejected:* chat-only scope; a CLI flag (`--prompt-format`); a runtime TUI
   switch via `TurnSettings`; per-call-site `if xml { to_xml() } else
   { to_markdown() }` chains — a single `PromptFormat::render` match is the
   one dispatch point, and scattering invites drift.

## Work

1. **Rewrite `agent/src/prompt.rs` as builders returning animist types.**
   ```rust
   pub struct Context<'a> {
       pub cwd: &'a str,
       pub platform: &'a str,
       pub date: &'a str,
       /// `config.agent.preamble` followed by the discovered instructions.
       pub instructions: &'a str,
   }

   pub fn system(ctx: &Context<'_>, plan: Option<&Path>) -> animist::agent::Prompt;
   pub fn research(ctx: &Context<'_>) -> animist::agent::Prompt;
   pub fn general(ctx: &Context<'_>) -> animist::agent::Prompt;
   pub fn subagent_role(kind: SubagentKind) -> animist::agent::Section; // `SubagentKind` becomes pub(crate)
   ```
   Keep `today_utc` and `civil_from_days` (both used by tests and the
   environment section). Delete `Slot`, `SlotKind`, `SlotEntry`,
   `ResolvedSlots`, `PromptId`, `ParseSlotError`, `ParsePromptIdError`,
   `Vars`, `assemble`, `assemble_raw`, `fill_marker`, `render_slot`,
   `render_efficient_tools`, `has_slot`, and the four `include_str!` template
   constants. Never emit an empty section; the builder decides which sections
   exist.
2. **Wire the chat path.** `ConversationBuilder` hands the prompt pieces
   (preamble + instructions, `cwd`, extra sections) to `RequestLayer`; the
   layer builds the `Prompt` at `turn_start` (run-scoped date), appends
   `Plan mode` when `settings.mode.plan_path()` is set, and stores the
   rendered string; keep `prepare_request`'s single assignment to
   `request.system`. Pin the mode switch: one conversation, two runs,
   different system bytes, no agent rebuild.
3. **Wire the periphery.** `subcmd::prompt` prints
   `prompt::system(..).to_markdown()`. The subagent launcher appends
   `subagent_role(kind)` as a `Section` instead of formatting it into
   `config.agent.preamble`; add a `ConversationBuilder::section(..)` if that is
   the smallest way to carry it. Convert `REVIEWER_SYSTEM_PROMPT`
   (`agent/src/auto_review.rs:22`) and `COMPACTION_SYSTEM`
   (`agent/src/compaction/llm.rs:18`) to `Prompt`s rendered with
   `to_markdown()`.
4. **Delete the templates.** Remove
   `agent/src/prompts/{system,research,general,plan}.md`, plus
   `compaction.md` once step 3 has a `Prompt` for it. Keep
   `compaction_user.md` and `compaction_targeted_user.md`. Update the
   `craft prompt` help/docs if any promise slot overrides.
5. **Wire the format option.**
   - `config.rs`: `PromptFormat` enum (serde lowercase `"markdown"`/`"xml"`),
     field on `AgentConfig` defaulting to `Markdown`, documented key in
     `agent/agent.example.bml` (`# prompt_format = "xml"` with a one-line
     comment), parse test for both values plus a rejected unknown value.
   - `prompt.rs`: `PromptFormat::render` match (or the enum lives here with
     `#[derive(Deserialize)]` — implementer's choice; doc names `prompt.rs` to
     keep config.rs free of rendering policy).
   - `harness/request.rs` / `harness/conversation.rs`: carry the value through
     `RequestLayerInputs.prompt_format`; render the chat prompt and the
     plan-mode variant through it.
   - `subagent.rs`, `auto_review.rs`, `compaction/llm.rs`: render the child /
     reviewer / compaction `Prompt`s through the same enum (children inherit
     the conversation's format via the builder, same as `preamble_base`).
   - `subcmd.rs`: `craft prompt` honors `config.agent.prompt_format`; add a
     `--format markdown|xml` preview flag to `subcmd::prompt` and its clap
     wiring (check `cli.rs` for the `PromptVariant` args definition).

## Tests

- Replace the nineteen `prompt.rs` tests with section-model tests: exact
  rendered bytes for build and plan mode, section order and count, no empty
  sections, efficient-tools composition, environment values, instructions
  appearing exactly once, research vs general section sets, and
  `to_xml()` producing one `<section>` per `Section`.
- Keep a byte-for-byte golden in `prompt.rs` and make
  `system_prompt_bytes_match_the_old_policy_output` (`harness/request.rs:269`)
  assert the *request's* system equals the golden for the same inputs — the
  request is the contract, not the helper.
- Keep `output_cap_clamp_counts_request_overhead`
  (`harness/tests/spec.rs:73`) measuring the same system bytes the request
  layer sends; update its construction to the new builder.
- Keep `run_turn_sends_the_resolved_preamble_exactly_once`
  (`tui/provider/live/tests.rs:932`) green: `BASELINE-PREAMBLE` and `MARKER`
  each appear exactly once in the captured body.
- Add one test that a plan-mode run's `RequestLayer` output contains the plan
  path and a build-mode run's does not, with the same conversation.
- An XML byte-for-byte golden for build and plan mode alongside the markdown
  ones; `system_prompt_bytes_match_the_old_policy_output` runs once per format
  (the request is the contract, both ways).
- Config parse: both values accepted, unknown value rejected, default is
  markdown.
- One test that `--format xml` output equals `prompt.to_xml()` and differs
  from the markdown golden.
- The plan-mode-plan-path-presence test asserts in the configured format.
- Reviewer/compaction prompts render through `PromptFormat::render`
  (a render-dispatch test over both enum variants suffices; no provider
  needed).

## Acceptance

- `grep` finds no `{{`, no `assemble`, no `ResolvedSlots`, no `Vars`, and no
  `include_str!` chat-prompt template in `agent/src/prompt.rs`, and
  `agent/src/prompts/` contains only the two compaction *user* templates.
- `agent/src/prompt.rs` names no craft prompt model type: only
  `animist::agent::{Prompt, Section}` leave it.
- Every system prompt in the crate is produced by the same builder family and
  installed through `request.system = prompt.to_markdown()`.
- Plan mode changes the prompt without rebuilding the `Agent`.
- `cargo fmt -p craft --check`, `cargo clippy -p craft --all-targets` and
  `cargo test -p craft` pass; the test-inventory diff lists each removed test
  with its replacement.
- `agent.example.bml` documents `prompt_format` with both values named.
- Markdown default: the markdown golden is byte-identical to the value pinned
  before this option landed (the option alone changes no bytes).
- Every `request.system =` and one-shot system prompt in the crate passes
  through `PromptFormat::render`; `grep` finds no direct `.to_markdown()` /
  `.to_xml()` call outside `prompt.rs` (tests excepted).
- The completion note lists the behaviour changes from
  [Decision 2](#decisions) and names the new goldens.

## Followups to file (non-blocking)

Write these in `~/Projects/animist/docs/followups/` if the migrated prompt
still reads badly; none may block this task:

1. `Section::to_markdown` always prints `# {name} - {title}`, so a heading-less
   prose block (`Identity`, `Instructions`, `Environment`) has to invent one.
   An optional heading (or a `title: Option<String>`) would let applications
   keep pure prose sections.
2. `Prompt::to_markdown` joins sections with a single newline, so headings sit
   directly under the previous section's last bullet. A blank-line separator
   would match how markdown is written by hand.
3. `Plan mode` needs a wrapper convention (`<system-reminder>`) that `Section`
   cannot express. If craft needs it back, ask for a raw/markup section variant
   rather than reintroducing a craft-side renderer.
