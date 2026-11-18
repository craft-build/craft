# Sandbox

The `bash` tool runs under an OS command sandbox configured by the `[sandbox]` section:

```bml
sandbox {
  enabled = true
  mode = "workspace_write"   # workspace_write (default) | read_only | danger_full_access | off
  network = true             # false blocks sandboxed network access
}
```

## Backends

- **macOS** — the stock `sandbox-exec`.
- **Linux** — argv is wrapped with `bubblewrap` (`bwrap`, prerequisite).

## Enforcement is fail-closed

On supported platforms, if the backend binary is missing while confinement is requested, the command errors before it starts — install it or set `sandbox.mode = "off"`. On platforms without a sandbox implementation, commands run unsandboxed with a visible one-time note.

## Escape hatches

- `--yolo` disables the sandbox entirely (predecessor parity).
- `CRAFT_SANDBOX=off` is the documented env escape hatch with highest precedence (debug/CI).

## Plan mode and read-only workers

Plan-mode turns and read-only workers (the reviewer subagent, research subagents) run bash read-only; the allocated plan file stays writable via an exact-path grant.

See [Permissions & Snapshots](./permissions.md) for the approval rules that sit on top, and [Configuration](./configuration.md) for where the block lives.
