# Craft codemode via Monty — design & implementation plan

Nov 8, 2026 · @Akari

## Summary

Craft gets one new tool, `code`, that runs model-written Python in Monty 1.x. Every tool call the script makes goes back through `ToolContext::dispatch_all`, exactly as `batch` does today. That keeps permissions, dedup, snapshots and persistence in force for every call. Guardrails judge the script as one call.

The earlier Monty integration (maki's `code_execution`, which the craft fork inherited) mostly failed inside the interpreter rather than in the tools. Its git history shows why: the calling convention fought the way models write Python. Return values were truncated or numbered text, positional arguments were rejected, every call needed `await`, and the runtime of that era was Monty 0.0.x.

The plan fixes the *contract* first and the engine second:

- Tools take positional and keyword arguments.
- File and search tools return clean, structured values.
- Scripts are type-checked against generated stubs before they run.
- Every failure comes back as one short, fixable message.

It ships behind a config flag in five phases. Phase 0 is a measured spike: does a corpus of real model-written scripts pass at 90% or better? Nothing is wired into the agent until it does.

## Lessons from maki

Most of maki's failures were contract mismatches between the model and the host, not Monty bugs, and each one has a direct fix here. The evidence is [maki's `maki-interpreter` crate](https://github.com/tontinton/maki/tree/main/maki-interpreter), [its `code_execution` plugin](https://github.com/tontinton/maki/blob/main/plugins/code_execution/init.lua) and the commit history behind both.

| Failure seen in maki | Evidence | What this design does instead |
| --- | --- | --- |
| Scripts parsed truncated, compacted tool output and got wrong answers silently | Fixed late by commits `22f81a82` and `17625618` ("Preserve complete intermediate tool results") | Calls from a script always get full output. File and search tools return structured values (§ Prompt and stub design) |
| Positional arguments raised an error: `read('a.py')` failed | `build_tool_input` rejects positional non-dicts | Map positional arguments onto the schema's parameter order |
| Every tool was async, so a missing `await` produced a coroutine object and an `AttributeError` far from the cause | Prompt says "you MUST await every call" | Type-check against `async def` stubs before running. `ty` flags `read(...).strip()` on the right line |
| `asyncio.gather` cancelled the sibling calls when one call failed | Commit `ca5c3378`, Monty 0.0.21 | Host-side `gather` that keeps partial results. Re-test on 1.x |
| `read` returned 200 numbered lines by default, so searches past line 200 missed silently | Commit `18c12ea1`. Craft's `read` has the same 200-line default | A codemode-only `read_text` returns the whole file without numbers |
| `os.listdir` and `open()` raised `PermissionError`, which looked like a real disk fault | Commit `4722c050` | OS calls get one explicit message pointing at `glob()` / `read_text()`. Never a fake disk error |
| MCP tools raised `NameError`, and hyphenated names were invalid Python | Commits `982f1082` and `40abe283` | Stubs come from the live registry, with aliases. Uncallable tools are listed as such |
| Two scripts deadlocked on a shared memory-limit mutex while waiting on a subagent | Commit `319b631a` | No global-allocator memory limit in-process. Memory caps arrive only with worker isolation (Phase 4) |
| A failed sync tool call aborted the whole run | `InterpreterError::ToolCall` path in `runner.rs` | Tool failures raise a catchable `ToolError` inside the script |
| Traceback line numbers pointed into the preamble | Fixed in `ca5c3378` | No preamble: helpers are host functions, so line numbers match the model's code |

Two more background facts. Maki ran Monty on `spawn_blocking` with a `flume` bridge, and that part worked. This design keeps the message protocol, but not necessarily the blocking thread (§ Architecture, Where the interpreter runs). Maki also moved from Monty 0.0.17 to 0.0.21 and then to crates.io 1.x (commit `70348ec0`), so many of the failures you saw came from pre-1.0 Monty.

## Goals and non-goals

The goal is a `code` tool that frontier and mid-size models use successfully on the first try in at least 90% of runs, with no new way around craft's policy.

**Goals**

- Loops, branching, filtering and fan-out across tools in one model turn, with only the printed or returned result entering context.
- Every nested call passes the same policy as a top-level call: permissions, `--disallowed-tools`, dedup, `/undo` snapshots, persistence. Doom-loop and per-tool guardrails judge the code call as a whole, not its children.
- Parallel calls (`gather`, or awaiting several pending calls) become one `dispatch_all` wave under the existing scheduling claims.
- Bounded time, recursion, output size and nested-call count. Cancellation stops the script promptly.
- Every failure is reported as a short, specific message that tells the model how to fix it.
- Measurable: every run is logged locally with its outcome and failure class.

**Non-goals**

- A general Python environment: no third-party packages, network or subprocesses. `bash` remains the tool for those.
- Direct filesystem access from Python. Files are reached only through tools, so permissions and the read lifecycle stay authoritative.
- State kept between `code` calls (a REPL). Each call starts fresh, and no REPL mode is planned (open question 5), not even through `MontyRepl`.
- Replacing `batch` or direct tool calls. Codemode is an addition that config can make the default per model.

## Architecture

The interpreter is stepped through its RunProgress states, and only craft's async side dispatches calls. Craft's async side owns the `ToolContext` and does all the dispatching. Monty never touches `ToolContext`, so its borrowed lifetime and `Send` bounds are never a problem.

&#91;embedded content: codemode architecture · worker thread, bridge, craft pipeline\]

The worker never holds `ToolContext`. It only sends batches of calls, and every call it makes passes through the highlighted `dispatch_all` and craft's layer chain like any other call. Preflight errors go straight back to the model without running anything.

### Components (new module `agent/src/codemode/`)

| File | Responsibility |
| --- | --- |
| `mod.rs` | `Code` tool: `ToolImpl` with `CodeArgs { code: String, timeout_secs: Option<u32> }`, exclusive like `batch` |
| `catalog.rs` | Builds the callable set from the registry's `ToolDefinition`s: Python names, aliases, parameter order, return adapters, the uncallable list |
| `stubs.rs` | Renders the catalog as Python stubs (`async def read_text(path: str) -> str: ...`) for the prompt and the type checker |
| `worker.rs` | Thread entry point. Owns `MontyRun` and the `RunProgress` loop. Sends `Msg::Calls`, `Msg::Print` and `Msg::Done` |
| `bridge.rs` | Async side: receives messages, turns calls into `ToolCall`s, calls `dispatch_all`, replies. Races everything against cancellation and the deadline |
| `convert.rs` | JSON ↔ `MontyObject`, taken almost verbatim from maki's `convert.rs` (it is well tested) |
| `adapters.rs` | Per-tool result shaping for scripts (text → `list[str]`, `list[Match]`, raw file text) |
| `diagnose.rs` | Classifies failures and writes the model-facing error plus a repair hint |
| `telemetry.rs` | Appends one JSONL record per run: model, outcome, failure class, call count, duration |

### The run loop

1. **Preflight (async side).** Reject code longer than 2,000 lines. Run `monty-type-checking` against the stubs. On type errors, return them and run nothing. Then parse and compile with `MontyRun::new`; a syntax error returns here.
2. **Start (worker).** `runner.start(...)` with a `ResourceTracker` (time and recursion limits) and a `PrintWriter` callback that streams lines to the bridge.
3. **`FunctionCall`.** Resolve the name in the catalog and map positional arguments to keywords. Unknown names resume with `NotFound`, which raises `NameError` listing the callable tools. Known names are queued with `resume_pending`.
4. **`ResolveFutures`.** Send every pending call as one `Msg::Calls` batch. The bridge builds `ToolCall`s with ids `code-<n>`, runs `context.dispatch_all(calls)`, applies the result adapters and replies. Failed results resume as a `ToolError` exception, not an abort.
5. **`NameLookup`.** Catalog names resolve to function objects, so `f = read_text` works. Anything else is `Undefined`.
6. **`OsCall`.** Raise `PermissionError` with "use `glob()` / `read_text()`". Sleep and time calls are served within the deadline.
7. **`Complete`.** Return the stdout tail plus `repr` of the final expression, both capped.

All calls are resolved through the pending path, even a single one, so there is a single code path for both styles. A script that never awaits fails at type-check time, not at run time.

### Where the interpreter runs

A thread is not a hard requirement. What is required is that a CPU-bound Python loop never stalls a tokio worker for long. Phase 0 picks one of three options:

1. **Inline on the tool's own task (preferred).** Drive `RunProgress` directly inside `Code::call`, with no thread and no channel. The interpreter only holds the worker while it executes; every tool wait is an ordinary `.await`. Keep the pure-CPU budget low (2 s per script) because codemode scripts spend almost all their time waiting on tools. This needs `RunProgress` to be `Send` if animist's tool futures must be `Send`, and Phase 0 checks that.
2. **A dedicated `std::thread` per script** with the `flume` bridge. This avoids the `spawn_blocking` pool and uses the same message protocol. Use it only if Phase 0 finds scripts that need seconds of pure CPU.
3. **The Phase 4 worker subprocess straight away.** No thread question at all, and crash isolation comes with it, at the cost of more up-front work.

If option 1 is chosen, the diagram's channel becomes plain function calls inside the task, and cancellation is just dropping the future.

## Prompt and stub design

The model sees real Python stubs with typed returns, and the same stubs are what the type checker enforces. What the model is told and what the runtime accepts therefore cannot drift apart.

### Calling convention

- Every tool is `async def` and must be awaited. That matches Anthropic's programmatic tool calling, the style frontier models are trained on. A missing `await` is caught by `ty` before execution, with the line number.
- Positional and keyword arguments both work. Positional order is required parameters first, then optional ones, in schema order. The stub shows that order.
- Top-level `await` is allowed, so there is no `async def main()` boilerplate.
- The final expression is returned. `print` output is streamed and returned too.
- `gather(*aws)` is a host builtin, also bound as `asyncio.gather`. It returns results in order, and a failed call becomes a `ToolError` value in its slot instead of cancelling the others. Pass `fail_fast=True` to raise on the first failure.

### Result adapters

Scripts receive values shaped for code. Model-facing tools keep their current text output.

| Tool in script | Returns | Notes |
| --- | --- | --- |
| `read_text(path)` | `str`, the whole file, no line numbers | New internal tool. It goes through the same permission rules and read lifecycle as `read`. 8 MiB cap |
| `read(path, offset=1, limit=200)` | `str`, numbered | Unchanged, for scripts that want `read`'s exact view |
| `glob(pattern, path=None)` | `list[str]` | Parsed from the tool's lines. Incompleteness raises `ToolError(truncated=True)` instead of returning a silent partial list |
| `grep(pattern, ...)` | `list[Match]`, with `Match` as a `TypedDict(path, line, text)` | Same incompleteness rule |
| `list(path)` | `list[str]` | Stub name `list_dir` so it doesn't shadow the Python builtin |
| `bash(command, ...)` | `BashResult(TypedDict: exit_code, stdout, stderr)` | Only when `bash` is in the registry. Still permission-gated per command |
| Edit-family, MCP and other tools | `str` | Text as today. MCP `structuredContent`, when present, is returned as a `dict` instead |

### What the tool description contains

The description is about 25 lines, built once per registry:

1. One sentence on when to use the tool (chained or dependent calls, filtering large output, fan-out) and when not to (a single call that needs no processing).
2. The rules above, as five bullets.
3. The allowed modules: `asyncio`, `collections`, `dataclasses`, `datetime`, `itertools`, `json`, `math`, `re`, `typing`. It also says: no classes with inheritance, no `match`, no `open()`.
4. Three short examples: a fan-out read with a filter, a grep aggregation, and an edit loop with error handling.
5. The stub block, plus a `Not callable here:` line for tools the registry holds but codemode excludes (`task`, `question`, `batch`, `code`).

For large MCP catalogs, the stubs list only names and one-line docstrings, and `list_tools` gives full signatures on demand. That keeps the description under a token budget (proposed: 3,000 tokens).

## Error handling and feedback loop

Every failure returns as one `is_error` tool result that starts with its class, names the line, and ends with one concrete fix. Each class also counts separately in telemetry, so we can see which ones dominate.

| Class | Detected at | Model sees (shape) | Repair hint |
| --- | --- | --- | --- |
| `syntax` | `MontyRun::new` | `SyntaxError line 4: ...` plus a 3-line excerpt | None needed; the excerpt is enough |
| `type` | `ty` preflight | Up to 5 diagnostics, each `line N: message` | Pattern hints, e.g. on a `CoroutineType` attribute error: "add `await` before `read_text(...)`" |
| `unsupported` | Preflight scan (`scan_imports`, class bases, `match`) or a Monty `NotImplementedError` | `Unsupported in codemode: import pandas` | Lists the allowed modules or says to drop the base class. Raised before running where possible |
| `unknown_tool` | `FunctionCall` lookup | `NameError: 'search_code' is not callable here` | Up to 3 closest names by edit distance, plus the not-callable list |
| `tool_error` | Uncaught `ToolError` | `ToolError from edit (line 12): old_string not found` | Hint: wrap the call in `try/except ToolError`, or use `gather`, if partial progress is acceptable |
| `runtime` | Other uncaught exception | A traceback trimmed to the model's own frames | None |
| `limit` | Deadline, recursion, nested-call cap or output cap | `Time limit 30s exceeded at line 8 after 41 tool calls` | Suggests narrowing the loop or raising `timeout_secs` (cap 120) |
| `cancelled` | User interrupt | `Cancelled after N tool calls` | None. The calls already completed stay in history |

### Partial progress is preserved

Nested calls are real child calls, so edits a script made before failing are already in history and snapshotted. The error result begins with a summary such as `Completed 7 of 9 tool calls before failing: edit x3 ok, read x4 ok`. The model can then continue without redoing work.

### Output shaping

- Stdout and the final value are capped at 32 KiB combined. Craft's existing compression pass handles anything larger between runs, with `retrieve` available.
- When stdout is empty and the final value is `None`, the result is `(no output: print or end with an expression)`. That stops the model re-running the script to see why nothing came back.
- Script line numbers match the model's code exactly, because there is no preamble.

### One retry is the norm

The hint text is written so the next attempt is a small edit to the same script, not a rewrite. Telemetry records whether the retry succeeded. That gives the main health metric: success within 2 attempts.

## Safety

Codemode grants no capability that a direct tool call doesn't already have. The script is just a different way of issuing calls, and every call still passes policy, dedup, snapshots and persistence.

**Permissions and policy**

- Calls go through `dispatch_all`, so the `PolicyLayer`, the headless permission gate, `--disallowed-tools`, plan mode and auto-review all apply per call. An interactive ask pauses the script while the user answers. The deadline clock stops during the wait.
- The callable set equals the registry the conversation already has. A read-only subagent's `code` tool therefore cannot reach `edit`, the same rule maki learned with tool audiences.
- `code` is excluded from its own catalog. `batch` is excluded as well, since `gather` covers it and craft's nesting rules reject double fan-out.
- The `code` call itself needs no permission prompt; the calls it makes are prompted individually. A `deny` rule on `code` turns codemode off entirely.

**Limits (defaults, all in config)**

| Limit | Default | Enforced by |
| --- | --- | --- |
| Interpreter CPU time (excluding tool waits) | 30 s, max 120 s | `ResourceLimits::max_feed_duration` per resume, plus a host deadline |
| Wall clock, including tools | 10 min | Bridge `select!` with a timeout |
| Recursion depth | 200 | `ResourceTracker` |
| Nested tool calls per script | 200 | Bridge counter. This is the only cap, because craft sets animist's max\_child\_calls to unlimited and so `ChildCallLimitExceeded` never fires |
| Calls per `gather` wave | `max_concurrent_tools` (8) | `dispatch_all` waves, no new knob |
| Output returned | 32 KiB | `diagnose.rs` |
| Source size | 2,000 lines | Preflight |

**Crash isolation**

In-process Monty can take craft down on a stack overflow or an engine bug, which would lose the session. The mitigation is staged:

1. Phases 1–3: the worker thread runs with an explicit 64 MiB stack (`std::thread::Builder::stack_size`) and recursion is capped well below it. The session log is crash-safe already, so the worst case is losing the turn in progress.
2. Phase 4: move the worker into a subprocess, `crafty __codemode-worker`, speaking the same message protocol over stdin/stdout. A crash then becomes a `MontyCrashed` error result. Memory caps become possible there too, by setting `monty-alloc` as the worker binary's global allocator or with `setrlimit`, without touching the main process. The alternative is to adopt the `monty-pool` crate directly, if its protocol fits.

**Filesystem, environment and network**

Scripts have none. `OsCall`s raise `PermissionError` with a pointer to the tool to use. Monty has no ambient host access, so this only shapes the error message; it isn't a guard against a leak. Time and date calls are served from the host clock, and `random` is seeded per run.

## Configuration and UX

Codemode is off by default and enabled per session or per model through a new `[codemode]` BML section. It can be added beside the normal tools, or set to replace them for models that do better with code.

```bml
codemode {
  mode = "alongside"      # off | alongside | exclusive
  timeout_secs = 30
  wall_clock_secs = 600
  max_tool_calls = 200
  max_output_kib = 32
  type_check = true
  telemetry = true        # local JSONL under the state dir
}

# per-model override in a model block
models."qwen3.6-27b" { codemode = "off" }
```

- `exclusive` advertises `code`, `question`, `todo_write` and `task` only. All other tools are reachable only from scripts, which needs the animist callers change in Phase 1. This is the configuration that saves the most tokens and the one to evaluate last.
- CLI: `--codemode off|alongside|exclusive` overrides config for one run, the same pattern as `--thinking`. A `/codemode` slash command toggles it in the TUI between runs. The agent is rebuilt on the same history, just as `/model` does.
- Subagents inherit the mode. The reviewer and research workers get `alongside` with their restricted catalog.

**TUI**

- A `code` card shows the script syntax-highlighted (syntect is already a dependency) and collapsed to its first 8 lines, then streamed `print` lines through `ToolContext::progress`.
- Nested calls render as indented child rows under the card, like `batch` children. The card footer reads `9 calls · 2.1 s · ok`.
- Errors show the failure class as a badge, so a user can tell type-check rejections from tool failures at a glance.

**Sessions and ACP**

- No new record types are needed: the `code` call and its children persist as normal entries with `parent_call_id`. Old builds can still read sessions that contain them.
- ACP surfaces the script as a tool call whose content is the code. The children appear as nested tool calls, the same way `batch` reports today.
- `--print --output-format stream-json` emits child calls as tool\_use events with a parent id, which keeps Claude Code compatibility.

## Testing and evaluation

The deciding test is a corpus of real model-written scripts replayed offline. Last time the integration never gave you a successful call, so nothing gets wired in until that corpus passes.

**1. Script corpus (Phase 0 onward)**

- About 150 scripts in `agent/tests/codemode/corpus/*.py`, each paired with a `.expect.toml` (expected tool calls, final value or failure class).
- Sources: generate them by prompting 4–5 models (two frontier, two mid-size, one local) with the draft tool description on 30 representative tasks. Add the failing scripts from maki's issue tracker. Hand-write a set of adversarial ones (infinite loops, deep recursion, huge output, banned imports, missing `await`).
- A mock dispatcher answers tool calls from fixtures, so the corpus runs in `cargo test` with no network.
- Gate: at least 90% of non-adversarial scripts run correctly on the first attempt, and 100% of adversarial scripts fail with the right class.

**2. Unit tests**

- `convert.rs`: round-trip tests ported from maki.
- `stubs.rs`: snapshot tests (craft already uses golden files) for the builtin catalog, an MCP catalog with hyphenated names, and a restricted subagent catalog.
- Positional-to-keyword mapping, adapters (glob, grep, MCP `structuredContent`) and each failure class's message format.

**3. Harness tests** (`src/harness/tests/`, next to `dispatch.rs`)

- A script's calls hit the permission gate. Denied calls raise `ToolError` and never execute.
- Plan mode blocks edits from scripts.
- `/undo` restores files edited by a script.
- A gather of 20 reads runs in at most 3 waves with `max_concurrent_tools = 8`.
- Cancellation mid-wave stops the worker within 1 s and leaves no thread behind.

**4. Live evaluation (Phase 5)**

- Extend the existing conformance suite (`src/conformance/`) with paired tasks run with codemode off, `alongside` and `exclusive`.
- Record per model and mode: task success, tokens, turns, wall time and codemode first-try success.

**Success metrics**

| Metric | Target to ship `alongside` by default | Target for `exclusive` |
| --- | --- | --- |
| Code runs succeeding first try (telemetry) | ≥ 85% | ≥ 90% |
| Succeeding within 2 attempts | ≥ 95% | ≥ 97% |
| Conformance task success vs codemode off | No regression | No regression |
| Tokens per solved task vs off | — | ≥ 25% lower |

Whichever failure class leads telemetry each week goes to the top of the backlog. Usually the fix is in the stubs, the description or an adapter, not in Monty.

## Implementation plan

There are five phases, each ending in a gate. Phase 0 decides whether the rest happens: it measures Monty 1.x against real model output before any agent code changes.

### Phase 0 — Spike and corpus (about 3 days)

Gate: the corpus pass rate is at least 90% with the mock dispatcher, or a written list of Monty gaps with workarounds.

- [ ] Add `monty`, `monty-types` and `monty-type-checking` to `[workspace.dependencies]`, pinned to exact versions. Check build time, binary size delta and `cargo deny`.
- [ ] Probe 1.x behaviour in a scratch test: top-level `await`, `asyncio.gather` with one failing call, positional arguments in `FunctionCall.args`, `TypedDict` returns, and whether `RunProgress` is `Send`.
- [ ] Build `tests/codemode/corpus/` with the generator script and mock dispatcher (§ Testing and evaluation).
- [ ] Draft the tool description and stubs, then generate the model scripts against that draft.

### Phase 1 — Working tool behind a flag (about 1 week)

Gate: harness tests green, and codemode usable in the TUI with `--codemode alongside`.

- [ ] Bump craft from animist 0.7 to 0.8.0.
- [ ] animist change: make a tool callable by child dispatch without advertising it to the model. Today `execute_tool_inner` (`src/agent/mod.rs`) runs a child only if its name is in the request's `visible` tool list. A hidden `read_text`, or any tool in `exclusive` mode, would come back as `Unknown tool: …`. Proposed fix: a `callers` field on `ToolDefinition` (`model`, `code`, or both, like Anthropic's `allowed_callers`). The request lists only `model` tools, and child dispatch checks the registry for `code` tools.
- [ ] `agent/src/codemode/{mod,catalog,stubs,worker,bridge,convert,adapters}.rs`.
- [ ] Internal `read_text` tool in `src/tools/read.rs`, callable only from scripts (`callers = code`), sharing `read`'s guards and read lifecycle.
- [ ] Register `Code` last in `Workspace::registry` and `subagent_registry` (`src/tools/mod.rs`), after MCP and `list_tools`, so its catalog snapshot from `ToolRegistry::definitions()` sees every tool.
- [ ] `CodemodeConfig` in `src/config.rs`, the `--codemode` flag in `src/cli.rs`, and the description text in `src/prompt.rs`.
- [ ] Claims: `Code` declares an exclusive claim, as `batch` does. animist's `dispatch` and `dispatch_all` refuse a non-exclusive parent with `ChildDispatchRequiresExclusive`.
- [ ] Cancellation: running inline, dropping the call future stops the script; with a thread, drop the bridge receiver so the worker unwinds.
- [ ] Guardrails (`src/harness/guardrail.rs`): record the ids of `code` calls in `pre_tool_exec`. For a child whose `parent_call_id` is one of them, skip doom-loop recording and denial, `check_before_call` and `record_result`, and doom success or error accounting, but still read and fill the dedup cache.

### Phase 2 — Diagnostics and preflight (about 4 days)

Gate: every adversarial corpus script fails with the right class, and the repair-hint retry passes for at least 80% of first-try failures.

- [ ] `ty` preflight with stubs, `scan_imports`, unsupported-syntax scan.
- [ ] `diagnose.rs` with the eight failure classes, edit-distance suggestions and the partial-progress summary.
- [ ] `telemetry.rs` writing JSONL to `paths::state_dir()/codemode.jsonl`, plus a `craft stats --codemode` summary.

### Phase 3 — TUI, ACP and print polish (about 4 days)

Gate: the code card and nested rows render in the TUI, ACP and stream-json, and a resumed session replays them correctly.

- [ ] Card renderer in `src/tui/provider/cards.rs`, progress streaming, failure-class badge.
- [ ] `/codemode` slash command, plus a session-persisted mode beside the thinking preference.

### Phase 4 — Crash isolation (about 1 week)

Gate: an adversarial stack-overflow script yields `MontyCrashed` and the session continues.

- [ ] `crafty __codemode-worker` hidden subcommand with a length-prefixed JSON protocol over stdio (`postcard` if JSON proves slow).
- [ ] Worker pool of 1–2 warm processes, with optional memory cap via `monty-alloc` in the worker or `setrlimit`.
- [ ] Evaluate the `monty-pool` crate first; adopt it if its protocol supports our call batching.

### Phase 5 — Evaluation and defaults (ongoing)

Gate: the metrics in § Testing and evaluation.

- [ ] Paired conformance runs, off vs `alongside` vs `exclusive`, across 4 models.
- [ ] Turn `alongside` on by default for models that meet the targets, using per-model config defaults.

## Risks and open questions

The biggest risk is that models keep writing Python that Monty rejects. Phase 0 measures that before committing, and the type-check plus hint loop is the fallback.

| Risk | Likelihood | Mitigation |
| --- | --- | --- |
| Monty's Python subset still rejects common model idioms | Medium | Phase 0 corpus measures it. Preflight turns it into fast, specific errors. Report gaps upstream |
| `ty` preflight adds latency or binary size (salsa database) | Medium | Measure in Phase 0. Cache one `TypeChecker` per conversation. Make it a config toggle |
| An engine bug crashes craft in-process | Low | Larger worker stack and recursion cap until Phase 4 isolation |
| A permission prompt in the middle of a script confuses users | Medium | The card shows "script waiting on approval" with the script's line number |
| Monty 1.x API churn (it reached 1.0 in late 2026) | Medium | Pin exact versions. All Monty types stay inside `codemode/` |
| Weaker local models misuse `await` | High | Per-model `codemode = "off"`. Sync calls were ruled out (open question 1), so async with await stays the only style |

**Open questions**

1. Decided: no sync calling style. Every tool stays async and must be awaited. Models that keep missing `await` get `codemode = "off"` instead.
2. Answered from animist 0.8.0: `ToolContext` does not expose the definitions. They sit in a crate-private `visible` field. `Code` therefore captures its catalog from the public `ToolRegistry::definitions()` when the registry is built, and MCP tools added mid-session arrive with the registry rebuild craft already does between runs. A public `ToolContext::visible_tools()` accessor upstream would be a small optional cleanup. The same code raised a bigger issue: child calls can only reach tools in the request's visible list (see Phase 1).
3. Decided: script child calls use the dedup cache (read and fill), but not the doom-loop or per-tool guardrails. One faulty script must not trip a circuit breaker on `read` or `edit` for the rest of the turn. The `code` call itself still goes through guardrails as a single call, so a model re-running the same failing script is still caught.
4. Decided: `read_text` stays codemode-only. It exists so a script gets the whole file as one clean string, with no line numbers and no 200-line page, which it can search or slice. The model's own `read` keeps its numbered, paged view.
5. Decided: no REPL mode. Keeping state across calls would make codemode a general Python environment, which is out of scope. Revisit only if a real need shows up.

**Sources**

- [pydantic/monty README](https://github.com/pydantic/monty) and [monty on docs.rs](https://docs.rs/monty/latest/monty/): `RunProgress` variants, `resume_pending`, `ResolveFutures`, limits, `monty-pool`, `monty-alloc`.
- [monty-type-checking on docs.rs](https://docs.rs/monty-type-checking/latest/monty_type_checking/): `TypeChecker`, stubs, `scan_imports`.
- [animist `ToolContext`](https://docs.rs/animist/latest/animist/agent/tool/struct.ToolContext.html): `dispatch`, `dispatch_all`, `progress`.
- [tontinton/maki](https://github.com/tontinton/maki): `maki-interpreter`, `plugins/code_execution`, and commits `22f81a82`, `17625618`, `ca5c3378`, `18c12ea1`, `4722c050`, `982f1082`, `319b631a`.
- Craft source: `agent/src/tools/batch.rs`, `agent/src/tools/mod.rs`, `agent/src/tools/read.rs`, `agent/src/config.rs`.
