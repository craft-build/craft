# Permissions & Snapshots

## Rules

Permission rules are BML blocks — allow / deny / ask — scoped to bash or MCP, with wildcards. They live in a `permissions.bml` next to the config or in a project's `.craft/` directory.

Bash commands are parsed with tree-sitter, so compound commands request each sub-command's approval separately: `git diff && rm -rf /` asks for `git *` and `rm *` approval separately. Subshells, command substitution, and pipes are handled the same way — most agents only see the first command.

## Every surface enforces

The TUI prompt overlay, the ACP `session/request_permission`, and the headless gate all consult the same `PermissionManager` built by `runtime::workspace_env`.

Headless runs (`--print`, `term run`, `recipe run`) enforce the same rules with no interactive approver: unresolved **asks fail closed** with guidance naming the opt-outs, and denied calls never execute. The gate covers batch children and subagent dispatch.

## Opt-outs

- `--yolo` / `--dangerously-skip-permissions` — bypass everything, disables the [sandbox](./sandbox.md).
- `-A` / `--auto-review` — an LLM (or decision-endpoint) reviewer answers permission prompts, fail-closed below a confidence threshold.

They are mutually exclusive and rejected at `Cli::validate`.

## Tool policy flags

- `--allowed-tools` **pre-approves** tools (scope-universal allow rules — every tool stays advertised; it does not restrict the tool set).
- `--disallowed-tools` **denies** tools at the permission gate (calls fail with a permission-denied message; batch children and subagents are covered).

Both accept PascalCase or snake_case native names (`Read`, `bash`) and MCP forms (`server__tool`, `mcp__server__tool`, `mcp__server` for a whole server). Unknown names error with the valid-tool list; a tool named by both flags is a hard error. The rules live for the process only — they never persist into sessions or `permissions.bml`.

`--permission-mode` maps to the same posture: `default`, `plan` (plan mode; conflicts with an explicit `--mode build`), or `bypassPermissions` (matches `--yolo`).

## Snapshots and /undo

Files are snapshotted before mutations — first-wins, ≤ 5 MiB, text only — and `/undo` walks them back.

Undo restores file contents only; deactivated history entries survive in session sources but are not restored.

## Auto-review mode

With `-A`, every permission prompt is answered by an LLM reviewer that sees the command and its context. Low confidence fails closed, so a confused reviewer can never silently approve a dangerous call. `/auto-review` toggles it from the TUI.
