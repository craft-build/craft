# Flow Mode

Flow mode is a multi-stage pipeline for larger features. Instead of one agent editing files directly, Flow breaks the work into named stages, each run by a sub-agent under a dedicated model role, and pauses for your approval before touching code.

It is built for the kind of task where you want a goal restated, a plan reviewed, and a verification report at the end. For small edits and quick questions, Build mode is faster and cheaper. Reach for Flow when a change spans many files or needs a clear acceptance bar.

## The stages

Each stage produces a document, persisted under your state directory, and feeds the next stage.

1. **Scout** reads the codebase and reports what it found.
2. **TPM** turns the request into a goal doc: restated goal, scope, and acceptance criteria. Flow pauses here for your approval.
3. **Plan** splits the goal into ordered chunks of work.
4. **Req** writes a spec for each chunk.
5. **Execute** implements a chunk. A general (write) sub-agent runs here.
6. **Review** checks the result. P0 or P1 findings send it back to Execute, up to `max_review_iterations`.
7. **QA** runs a final pass. A failure sends it back to Execute, up to `max_qa_iterations`.
8. **Integrator** merges parallel chunks (a no-op when `parallel_chunks` is 1).
9. **Verifier** checks the result against the acceptance criteria and returns a `ship` or `block` verdict.

## Run it

```bash
craft flow "add a FlowPanel component to the TUI that lists chunk statuses"
```

The first run reaches the approval gate and prints the goal doc. Resume by passing the session id and an approval payload:

```bash
craft flow -s <session-id> -p approved
```

To revise the goal instead of approving it, pass the new text:

```bash
craft flow -s <session-id> -p "narrow the scope to just the panel rendering, no keybinding yet"
```

### Print the outcome as JSON

`--print --output-format json` emits one JSON object with a `stop_reason` field. The approval gate reports `stop_reason: "awaiting_goal_approval"`; a completed run reports `stop_reason: "done"`.

```bash
craft flow "..." --print --output-format json
```

## Model roles

Each stage runs under a named role, resolved from `model_roles.toml`:

| Stage | Role |
| --- | --- |
| Scout | `scout` |
| TPM | `tpm` |
| Plan | `plan` |
| Req | `req` |
| Execute | `execute` |
| Review | `review` |
| QA | `qa` |
| Integrator | `integrator` |
| Verifier | `verifier` |

Unset roles fall back to your active model, so Flow works with zero role config. To steer cost, set the cheap stages (Scout, Req) to a small model and the hard stages (Execute, Verifier) to a strong one. See [Configuration](./configuration.md).

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
      semantic_index = true,
    },
  },
})
```

See the [Configuration reference](./configuration.md) for the full `agent.flow` field list and defaults.

## Semantic search

With `agent.flow.semantic_index = true`, Flow maintains a flat embedding index per workstream and exposes a `flow_search` tool to its stage agents. A stage can then ask "which of my prior documents is most relevant to X?" and pull the goal's acceptance criteria or a chunk's spec back into view without re-reading everything.

The index only activates once a workstream exceeds a small document threshold. Below that, `flow_search` falls back to a linear scan, so small workstreams pay no embedding cost.

Semantic search needs the `onnx` feature (the default build). Without it, `flow_search` is absent from the tool set entirely.

## TUI

In the TUI, Flow mode shows a chunk panel (toggle with `Ctrl+T`) listing each chunk and its status: queued, running, needs review, blocked, or done. Chunk status updates stream in as stages advance.

## Garbage collection

Old workstream directories accumulate under your state directory. Prune them with:

```bash
craft flow gc --older-than 30d
```

The age accepts `d`, `h`, `m`, `s` suffixes (for example `12h`, `45m`).

## When to use Flow vs Build

- **Build** for small edits, single-file changes, quick exploration, and interactive back-and-forth.
- **Flow** for multi-file features that benefit from a stated goal, a review loop, and a verification report.

Flow has higher per-task overhead because it runs many stages. If a chunk turns out to be small, the docs below will steer you back to Build.
