# Permissions & Snapshots

Permission rules decide whether a tool call is allowed, denied, or needs approval. The [sandbox](./sandbox.md) is a separate layer that limits what shell commands can access. Configure both for the level of access you want.

## Rules

Rules use BML blocks with `allow`, `deny`, or `ask` settings. They can apply to native tools or MCP tools, with scope patterns for commands and paths.

For project-specific rules, create `.craft/permissions.bml`. For example:

```bml
bash {
  allow = ["git status", "git diff *"]
  deny = ["rm *"]
}
```

This allows the matching status/diff commands and denies matching `rm` commands. It is an example policy, not a complete safety policy.

Global rules go in a `permissions` block in the [global configuration](./configuration.md), including split files such as `~/.config/craft/permissions.bml`. Project files use tool blocks at the top level, without the outer `permissions` block.

Bash commands are parsed with tree-sitter. A compound command such as `git status && git diff` is checked by subcommand, not just by its first command. The parser also handles subshells, command substitution, and pipes.

## How approval works

The same rule engine is used by the TUI, ACP, and headless runs:

- **TUI:** calls needing approval open a permission prompt.
- **ACP:** the client presents permission requests.
- **Headless** (`--print`, `term run`, `recipe run`): there is no interactive approver, so unresolved asks block the call (“fail closed”). Denied calls never execute.

These checks also cover batch children and subagent dispatch.

## Automated approval and bypass

- `-A` / `--auto-review` asks an LLM or decision-endpoint reviewer to answer permission prompts. Low confidence blocks the call. It does not remove the sandbox.
- `--yolo` / `--dangerously-skip-permissions` bypasses permission prompting **and disables the sandbox**.

The two options cannot be combined; Craft rejects the combination at startup.

## Tool policy flags

- `--allowed-tools` **pre-approves** tools (scope-universal allow rules — every tool stays advertised; it does not restrict the tool set).
- `--disallowed-tools` **denies** tools at the permission gate (calls fail with a permission-denied message; batch children and subagents are covered).

Both accept PascalCase or snake_case native names (`Read`, `bash`) and MCP forms (`server__tool`, `mcp__server__tool`, `mcp__server` for a whole server). Unknown names error with the valid-tool list; a tool named by both flags is a hard error. The rules live for the process only — they never persist into sessions or `permissions.bml`.

`--permission-mode` maps to the same posture: `default`, `plan` (plan mode; conflicts with an explicit `--mode build`), or `bypassPermissions` (matches `--yolo`).

## Snapshots and /undo

Craft snapshots text files before supported file-tool mutations. The first snapshot of a file is kept; files larger than 5 MiB are not snapshotted. Use `/undo` in the TUI to restore snapshotted contents.

Undo restores file contents only. It is not a general rollback for shell commands, external services, or every effect of an agent run. It also does not restore deactivated conversation entries, although those entries remain in the saved session sources.

## Auto-review mode

With `-A`, a reviewer sees the command and its context and answers permission prompts. Low confidence blocks the call, but a model's approval is not a guarantee of safety. `/auto-review` toggles the reviewer in the TUI.

Auto-review can also be the default posture — no flag needed. In the global config (`~/.config/craft.bml` or a split `~/.config/craft/*.bml`, e.g. a `permissions.bml` there), set it inside the `permissions` block:

```bml
permissions {
  auto_review = true
}
```

A project's `.craft/permissions.bml` sets the same key at the top level, beside its tool blocks:

```bml
auto_review = true
```

The project value takes precedence over the global value. The TUI and headless runs enable the reviewer when either the flag or configuration requests it. There is no command-line off flag; `/auto-review` toggles it for the current TUI session.

ACP does not install a local reviewer. Permission requests still go to the client even when auto-review is enabled in configuration.
