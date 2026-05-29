# Porting from upstream maki to craft

This guide describes how to apply upstream maki patches to craft.

Craft is a fork of [maki](https://github.com/tontinton/maki.git) by Tony Solomonik. When porting changes from upstream, you need to translate maki references to craft equivalents.

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

## Directory mapping

| maki path | craft path |
|---|---|
| `maki-agent/` | `craft-agent/` |
| `maki-config/` | `craft-config/` |
| `maki-config-macro/` | `craft-config-macro/` |
| `maki-docgen/` | `craft-docgen/` |
| `maki-highlight/` | `craft-highlight/` |
| `maki-interpreter/` | `craft-interpreter/` |
| `maki-lua/` | `craft-lua/` |
| `maki-markdown/` | `craft-markdown/` |
| `maki-providers/` | `craft-providers/` |
| `maki-storage/` | `craft-storage/` |
| `maki-tool-macro/` | `craft-tool-macro/` |
| `maki-ui/` | `craft-ui/` |
| `src/main.rs` | `src/main.rs` (unchanged) |

## Find-replace rules for patch translation

When applying a git commit/patch from upstream maki:

1. **Directory paths**: `maki-agent/` -> `craft-agent/`, etc. (in file paths in the diff header)
2. **Crate names in Cargo.toml**: `maki-*` -> `craft-*`
3. **Rust imports**: `use maki_*` -> `use craft_*`
4. **Rust crate references**: `maki_agent::` -> `craft_agent::`, `maki_config::` -> `craft_config::`, etc.
5. **Lua namespace**: `maki.` -> `craft.` in .lua files and embedded Lua strings in Rust
6. **Config dir**: `.maki` -> `.craft` in string literals
7. **CLI name**: `"maki"` -> `"craft"` in string literals (but NOT in URLs or attribution)
8. **Function names**: `create_maki_global` -> `create_craft_global`
9. **Variable names**: `maki` local variables -> `craft` in craft-lua runtime
10. **Thread names**: `"maki-lua"` -> `"craft-lua"`
11. **Do NOT change**: `github.com/tontinton/maki` URLs (attribution), author names

## Workflow for porting a commit

1. Get the patch from upstream: `git fetch upstream && git log upstream/main`
2. Generate patch: `git format-patch -1 <sha> --stdout > patch.diff`
3. Translate directory paths in diff headers: replace each `maki-*/` with `craft-*/`
4. Translate crate names in file content: `maki-agent` -> `craft-agent`, etc.
5. Translate Rust imports: `maki_` -> `craft_`
6. Translate Lua namespace: `maki.` -> `craft.` in .lua files (but NOT in `github.com/tontinton/maki` URLs)
7. Translate config dir: `.maki` -> `.craft` in string literals
8. Translate CLI name: `"maki"` -> `"craft"` in string literals
9. Remove `maki.sh` URL references (homepage is removed)
10. Apply: `git am patch.diff`
11. Resolve conflicts, run `cargo clippy` and `cargo nextest run`
12. Check for any missed references with `grep -r "maki" --include="*.rs" --include="*.toml"`

## Verification

After porting a patch, verify with:

```sh
cargo clippy --all-features --all --tests -- -D warnings
cargo nextest run --all-features --workspace
just gen-docs-check
grep -r "maki" --include="*.rs" --include="*.toml" --include="*.lua"
```

Any remaining `maki` hits should be attribution only (URLs, author names in Cargo.toml).
