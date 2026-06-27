---
name: port-maki-commit
description: Port a git commit from upstream maki into craft, translating names and obeying craft's tool-placement rules
when_to_use: When the user asks to port, import, or apply an upstream maki commit (or patch) into craft
---

# Port a maki commit into craft

Input: one upstream maki commit SHA (or a range). Output: the same change applied to craft, building and passing tests.

Craft is a fork of [maki](https://github.com/tontinton/maki.git) by Tony Solomonik. Porting means translating names, then deciding whether to keep maki's tool placement or diverge.

## Steps

1. Confirm the commit. Ask the user for the SHA if not given. Fetch the diff from github repository at https://github.com/tontinton/maki

2. Gather context for the craft code to understand what changes will be needed to implement the same changes of the maki diff inside craft.

3. Manually implement the change inside of craft

4. For any tool touched by the commit, run the placement check below and diverge from maki if the commit moves a tool the wrong way. Note the reason in the commit message when you diverge.

5. Create a conventional git commit and credit the maki commits authors

## Crate mapping

| maki crate | craft crate |
|---|---|
| `maki` (binary) | `craft` (binary) |
| `maki-agent` | `craft-agent` |
| `maki-config` | `craft-config` |
| `maki-config-macro` | `craft-config-macro` |
| `maki-docgen` | `craft-docgen` |
| `maki-highlight` | `craft-highlight` |
| `maki-interpreter` | `craft-interpreter` |
| `maki-lua` | `craft-lua` |
| `maki-markdown` | `craft-markdown` |
| `maki-providers` | `craft-providers` |
| `maki-storage` | `craft-storage` |
| `maki-tool-macro` | `craft-tool-macro` |
| `maki-ui` | `craft-ui` |

The directory mapping is identical: `maki-<x>/` -> `craft-<x>/`. `src/main.rs` stays unchanged.

## Do not change (attribution)

Never rewrite these, even though they contain the word "maki":

- `github.com/tontinton/maki` URLs (attribution links).
- Author names in `Cargo.toml` or commit trailers.
- `maki.sh` references: drop the URL only, keep attribution.

## Placement check: Rust or Lua

The heavy work in Lua plugins already runs in Rust (`craft.fs`, `craft.net`, `craft.treesitter`, `craft.fn`). Lua is the orchestration and presentation layer. So this check is about where the glue lives, not where the compute happens.

Keep a tool in **Rust** when it:

- Shares expensive state across tools, like tree-sitter parsers or symbol tables (example: `outline` and `callgraph` share `extract_symbols`).
- Does correctness-critical parsing or mutation that needs `Result` and type safety (example: `apply_patch`, `edit`, `multiedit`).
- Needs engine-level capabilities that cannot cross the Lua boundary cleanly, like ONNX embeddings or the monty Python sandbox.
- Is called by other tools in a tight loop, to avoid compounding the Lua thread hop (example: `batch`, `task`).

Keep a tool in **Lua** when it:

- Is a leaf I/O operation whose cost is the operation, not the glue (example: `bash`, `webfetch`, `glob`).
- Is mostly result presentation or formatting (example: `grep` result views, `question` forms).
- Is a natural user extension point or benefits from hot-reload (example: `skill`, `memory`, and the per-language extraction in `index`).

When a maki commit moves a tool against these rules, keep craft's placement instead of copying maki's choice.

## Verification

After applying and resolving, run:

```sh
cargo clippy --all-features --all --tests -- -D warnings
cargo fmt --all
just gen-docs-check
grep -r "maki" --include="*.rs" --include="*.toml" --include="*.lua"
```
Avoid full workspace test runs which tend to be really slow. Only use targetted test runs.
Any remaining `maki` hits from the grep must be attribution only (URLs, author names). If anything else fails or the grep shows non-attribution matches, fix before reporting done.
