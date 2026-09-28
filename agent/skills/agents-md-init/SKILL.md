---
name: agents-md-init
description: Author or refresh a minimal AGENTS.md project-instruction file that gives an agent just enough context to work effectively, with every line earning its place
when_to_use: When the user asks to create, bootstrap, write, or improve the AGENTS.md (or project instructions) for a repository
---

# Author a minimal AGENTS.md

`AGENTS.md` is the file an agent reads first to orient in a repo. Its job is to prevent mistakes, not to document everything. A bloated AGENTS.md costs tokens every session and gets skimmed, which defeats the purpose.

## Steps

1. **Explore the repo.** Read the manifest files (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`), the build commands (`Makefile`, `justfile`), the CI config, and any existing docs or README. You are looking for: how to build, how to test, where the entry points are, and the non-obvious gotchas.

2. **Interview for gaps.** Ask the user only what you cannot derive: project-specific conventions that matter, decisions that look wrong but are intentional, traps that cost you a run to discover. Do not ask things `ls` and `cat` can answer.

3. **Write a minimal `AGENTS.md`.** Cover:
   - What the project is (one or two lines).
   - How to build, lint, and test (the exact commands).
   - Architecture in broad strokes (the key crates/modules/dirs and how they connect).
   - Conventions that differ from the obvious (naming, error handling, where new code goes).
   - Gotchas that would cause an agent to make a mistake.

4. **Apply the minimal-line test.** Read every line and ask: "would removing this cause the agent to make a mistake?" If the answer is no, cut it.

## What to exclude

These are dead weight. An agent can derive or read them on demand, so putting them in `AGENTS.md` just burns tokens every session:

- **File-by-file structure.** `ls` and `outline` exist. The agent will read the tree when it needs to.
- **Standard conventions.** "Use descriptive variable names," "handle errors," "write tests" are true of every project. They add nothing specific.
- **Generic advice.** Anything that could appear in any repo's AGENTS.md unchanged is not pulling its weight here.
- **Frequently-changing info.** Version numbers, dependency lists, TODO lists. They rot and mislead.
- **Anything a fresh session could reconstruct** via `ls`, `cat`, or reading the manifest. If it is one command away, do not inline it.

The test is the same throughout: if a fresh session could derive it in seconds, it does not belong here. Reserve `AGENTS.md` for what an agent would get wrong without it.

## Keep it current

When you finish a task that revealed a new gotcha or convention, add the one line that captures it. Edit existing lines only when they steered you wrong. Do not rewrite the file for style.
