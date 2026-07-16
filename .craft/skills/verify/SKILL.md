---
name: verify
description: Verify that a code change actually does what it is supposed to by exercising it end-to-end and observing behavior at its real runtime surface, not by running tests or typechecking
when_to_use: When the user asks to verify, check, or confirm that a nontrivial code change works, before committing. Do not use for diffs that only touch tests, docs, or type declarations with no runtime surface
---

# Verify a change by driving the real surface

**Verification is runtime observation.** You build the app, run it, drive it to where the changed code executes, and capture what you see. That capture is your evidence. Nothing else is.

**Don't run tests. Don't typecheck.** Running them here proves you can run CI, not that the change works. Not as a warm-up, not "just to be sure," not as a regression sweep after. The time goes to running the app instead.

**Don't import-and-call.** `import { foo } from './src/...'` then logging the result is a unit test you wrote. The function did what the function does; you knew that from reading it. The app never ran. Whatever calls `foo` in the real codebase ends at a CLI, a socket, or a window. Go there.

## Find the change

The scope is what you are verifying, usually a diff. Establish the full range:

```bash
git diff @{u}.. --stat          # committed vs upstream
git diff origin/HEAD... --stat  # no upstream set
git diff HEAD --stat            # uncommitted working tree
```

State the commit count. No repo and no diff means the scope is whatever the user named; ask if they did not. **The diff is ground truth. Any description is a claim about it.** Read both. If they disagree, that is a finding.

## Surface

The surface is where a user meets the change. That is where you observe.

| Change reaches | Surface | You |
|---|---|---|
| CLI / TUI | terminal | type the command, capture the pane |
| Server / API | socket | send the request, capture the response |
| GUI | pixels | drive it under a headless runner, screenshot |
| Library | package boundary | sample code through the public export |

An internal function is not a surface. Something in the repo calls it and that caller ends at one of the rows above. Follow it there.

**No runtime surface at all** (docs-only, type declarations with no emit, build config with no behavioral diff): report SKIP with a one-line reason. Do not run tests to fill the space. Tests in the diff are the author's evidence, not a surface; a tests-only change is SKIP.

## Drive it

Take the smallest path that makes the changed code execute:

- Changed a flag? Run with it.
- Changed a handler? Hit that route.
- Changed error handling? Trigger the error.
- Changed an internal function? Find the CLI command, request, or render that reaches it. Run that.

Read your plan back before running. If every step is build / typecheck / run test file, you have planned a CI rerun, not a verification. Find a step that reaches the surface or report BLOCKED.

End-to-end, through the real interface. Pieces passing in isolation does not mean the flow works; seams are where bugs hide. If users click buttons, test by clicking buttons.

**Destructive path?** If the change touches code that deletes, publishes, sends, or writes outside the workspace and there is no dry-run or safe target, do not drive it live. Verify what you can around it and say which path you did not exercise and why.

## Push on it

The claim checked out is the first half, not the job. The description is what the author intended; your value is what they did not. Probe around it, at the same surface you just drove:

- **New flag / option**: empty value, passed twice, combined with a conflicting flag, typo'd.
- **New handler / route**: wrong method, malformed body, missing required field, oversized payload.
- **Changed error path**: the adjacent errors it did not touch. Did the refactor catch them too, or only the one in the diff?
- **Interactive / TUI**: Ctrl-C mid-op, resize the pane, paste garbage, rapid-fire the key, Esc at the wrong moment.
- **State / persistence**: do it twice, do it with stale state underneath, do it in two sessions at once.

Pick the ones the change points at. At least one push is required. A Steps list that is all happy-path is a replay, not a verification. A probe that finds nothing is still a step worth recording.

## Capture

Stdout, response bodies, screenshots, pane dumps. Captured output is evidence; your memory is not. Something unexpected? Do not route around it. Capture, note, decide if it is the change or the environment. Unrelated breakage is a finding, not noise.

Isolate shared process state (tmux sockets, ports, lockfiles). Use a unique tmux socket name, a fixed bind port, a temp dir.

## Report

Inline, final message:

```
## Verification: <one-line what changed>

**Verdict:** PASS | FAIL | BLOCKED | SKIP

**Claim:** <what it is supposed to do; note any mismatch with the diff>

**Method:** <how you got a handle and what you launched>

### Steps
Each step is one action against the running app and what it showed.
1. <what you did> -> <what you observed> <evidence: the app's own output>

### Findings
<Friction, surprises, anything a first-time user would trip on. Not just bugs.
Each probe gets a line even when it held. Lead with anything worth interrupting for.>
```

**Verdicts:**
- **PASS** — you ran the app, the change did what it should at its surface.
- **FAIL** — you ran it and it does not, or it breaks something else, or claim and diff disagree materially.
- **BLOCKED** — could not reach a state where the change is observable (build broke, env missing a dep). Not a verdict on the change. Say exactly where it stopped.
- **SKIP** — no runtime surface exists. One line why.

No partial pass. "3 of 4 passed" is FAIL until 4 pass or are explained away. **When in doubt, FAIL.** A false PASS ships broken code; a false FAIL costs one more human look.
