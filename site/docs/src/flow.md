# Flow Mode

Flow mode is Build mode plus a mutable turn type and a typed log. There is no orchestrator. The agent starts at `General` and runs the normal loop you already know from Build. At each turn boundary it may shift into a narrower turn type, and the pipeline shape (Scout, TPM, Plan, Execute, Review, Verifier) emerges from those shift choices rather than from a fixed driver.

It is built for the kind of task where you want a goal restated, a plan reviewed, and a verification report at the end. For small edits and quick questions, Build mode is faster and cheaper. Reach for Flow when a change spans many files or needs a clear acceptance bar.

## How it works

Flow mode reuses the Build-mode agent loop. The only differences are:

1. The session carries a mutable **turn type** (`General`, `Scout`, `Tpm`, `Plan`, `Req`, `Execute`, `Review`, `Qa`, `Report`, `Integrator`, `Verifier`). Each type declares what it reads, what it writes, which tools it may use, and which types may follow it.
2. The model **shifts** between types at turn boundaries via a dedicated `shift` tool. The shift is checked against the current type's declared transition rules: it may be accepted, blocked (an objective gate failed), or rejected as illegal (the current type does not allow that target).
3. Every turn commits one distilled entry to a **typed log** persisted under your state directory. The log is the workstream's source of truth, and the agent reads it back to resume.

If the model never shifts, Flow mode just runs as `General`, which is correct: some requests do not need a pipeline.

## The turn types

Each turn type is a behavior profile. The root-thread types are reachable from `General` and form the pipeline's spine:

- **General** is the entry point. Broad read access, no constraints, self-report transitions into the pipeline stages.
- **Scout** reads the codebase and writes a codebase-context entry.
- **Tpm** turns the request into a goal doc: restated goal, scope, and acceptance criteria. Flow pauses here for your approval.
- **Plan** splits the goal into chunks and writes the plan doc.
- **Integrator** merges the chunks' work.
- **Verifier** checks the result against the acceptance criteria and returns a `ship` or `block` verdict.

The per-chunk types live under child threads spawned by the `task` tool:

- **Req** writes a spec for one chunk.
- **Execute** implements the chunk.
- **Review** checks the result. P0 or P1 findings send it back to Execute.
- **Qa** runs a final pass.
- **Report** writes the chunk's outcome summary.

Because child threads run their own loop with the same shift machinery, each chunk can shift through Req, Execute, Review, and Qa independently. Parallelism is the model's job: it calls `task` multiple times via `batch` when chunks are independent.

## Run it

```bash
craft flow "add a FlowPanel component to the TUI that lists turn-type state"
```

The first run reaches the approval gate and prints the goal doc. Resume by passing the workstream id and an approval payload:

```bash
craft flow -s <workstream-id> -p approved
```

To revise the goal instead of approving it, pass the new text:

```bash
craft flow -s <workstream-id> -p "narrow the scope to just the panel rendering, no keybinding yet"
```

The workstream id is printed at the start of the run. Re-running with `-s <workstream-id>` reopens the same typed log; the agent reads its own past entries to pick up where it left off.

### The approval gate

When the model shifts from `Tpm` to `Plan`, the run ends with `stop_reason: "awaiting_goal_approval"` and emits the goal doc. Nothing runs further until you approve or revise.

On resume, the agent re-enters at `General` and reads the persisted goal from the typed log to re-derive the shift into `Plan`. No state beyond the typed log is needed to resume.

### Print the outcome as JSON

`--print --output-format json` emits one JSON object with a `stop_reason` field. The approval gate reports `stop_reason: "awaiting_goal_approval"`; a completed run reports `stop_reason: "done"`.

```bash
craft flow "..." --print --output-format json
```

## Model roles

Each turn type resolves to a named role from `model_roles.toml`, falling back to your active model when the role is unset. The role names mirror the turn types: `scout`, `tpm`, `plan`, `req`, `execute`, `review`, `qa`, `integrator`, `verifier`.

To steer cost, point the cheap types (Scout, Req) at a small model and the hard types (Execute, Verifier) at a strong one. With zero role config, every type uses your active model, so Flow works out of the box. See [Configuration](./configuration.md).

## Reading the typed log

The agent consults its own past entries through the `flow_search` tool. It asks "which of my prior documents is most relevant to X?" and pulls the goal's acceptance criteria or a chunk's spec back into view without re-reading everything. This is also how resume works: the next run reads the goal and plan from the log and continues.

`flow_search` is available only in Flow mode.

## TUI

In the TUI, Flow mode shows a compact status-line hint like `flow · scout · 2/3` while a run is in progress: the current turn type, plus running and total counts of child threads spawned by the `task` tool. The counts update live as `shift`, thread-spawn, and thread-exit events stream in. Toggle the todo/plan panel with `Ctrl+T` as usual.

## Configuration

Flow is configured under the `agent.flow` table in `init.lua`:

```lua
craft.setup({
  agent = {
    flow = {
      enabled = true,
      max_review_iterations = 3,
      max_qa_iterations = 2,
      parallel_chunks = 1,
    },
  },
})
```

See the [Configuration reference](./configuration.md) for the full `agent.flow` field list and defaults.

## Garbage collection

Old workstream directories accumulate under your state directory. Prune them with:

```bash
craft flow gc --older-than 30d
```

The age accepts `d`, `h`, `m`, `s` suffixes (for example `12h`, `45m`).

## When to use Flow vs Build

- **Build** for small edits, single-file changes, quick exploration, and interactive back-and-forth.
- **Flow** for multi-file features that benefit from a stated goal, a review loop, and a verification report.

Flow has higher per-task overhead because the model shifts through several types and persists a typed log. If a turn turns out to be small, the model can stay in `General` or shift back, which keeps the cost down.
