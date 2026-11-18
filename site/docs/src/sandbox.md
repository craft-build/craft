# Sandbox

The sandbox limits the files and network resources that the `bash` tool can access. It is separate from [permission rules](./permissions.md), which decide whether a tool call may run at all.

Configure it with a `sandbox` block in your [BML configuration](./configuration.md):

```bml
sandbox {
  enabled = true
  mode = "workspace_write"   # workspace_write (default) | read_only | danger_full_access | off
  network = true             # false blocks sandboxed network access
}
```

`workspace_write` is the default mode. Use `read_only` to restrict writes, or `danger_full_access` / `off` when confinement is not wanted. `network = false` blocks sandboxed network access.

## Platform requirements

- **macOS** — the stock `sandbox-exec`.
- **Linux** — install `bubblewrap` (`bwrap`); Craft uses it to wrap commands.

## Enforcement is fail-closed

On supported platforms, if the backend binary is missing while confinement is requested, the command errors before it starts — install it or set `sandbox.mode = "off"`. On platforms without a sandbox implementation, commands run unsandboxed with a visible one-time note.

## Escape hatches

- `--yolo` disables the sandbox and bypasses permission prompts.
- `CRAFT_SANDBOX=off` disables the sandbox with highest precedence, for example during debugging or CI.

Both remove OS confinement. Use them only when you intend commands to run without that protection.

## Plan mode and read-only workers

Plan-mode turns and read-only workers (the reviewer subagent, research subagents) run bash read-only; the allocated plan file stays writable via an exact-path grant.

See [Permissions & Snapshots](./permissions.md) for the approval rules that sit on top, and [Configuration](./configuration.md) for where the block lives.
