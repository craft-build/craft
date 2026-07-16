---
name: stuck
description: Diagnose a frozen, hung, or pegged-CPU process by inspecting OS-level process state (CPU, RSS, process state, child processes) without killing or signaling anything
when_to_use: When the user reports that a process, session, build, or command is frozen, hung, spinning, or unresponsive, and wants a diagnosis of why
---

# Diagnose a stuck process

A process looks frozen, hung, or pegged. Investigate from the OS level. **This is diagnostic only.** Do not kill, signal, or restart anything unless the user explicitly asks.

## Signs of a stuck process

| Signal | Likely cause | Confirm |
|---|---|---|
| High CPU (>=90%) sustained | infinite loop | sample twice, 1-2s apart, to rule out a transient spike |
| Process state `D` (uninterruptible sleep) | I/O hang (disk, network, NFS) | first char of the `state` column in `ps`; ignore modifiers like `+`, `s`, `<` |
| Process state `T` (stopped) | accidental Ctrl+Z | `ps` state column |
| Process state `Z` (zombie) | parent not reaping children | `ps` state column |
| RSS >= 4GB | memory leak making the process sluggish | `ps` RSS column |
| Stuck child subprocess | a hung `git`, `node`, or shell child freezing the parent | `pgrep -lP <pid>` for each suspect |

## Investigation steps

1. **List suspect processes.** Show CPU, RSS, uptime, state, and command:

   ```bash
   ps -axo pid=,pcpu=,rss=,etime=,state=,comm=,command=
   ```

   Filter to the process the user named, or to high-CPU / high-RSS rows if they did not.

2. **For anything suspicious, gather context:**
   - Child processes: `pgrep -lP <pid>` — a hung child can freeze the parent.
   - If high CPU: sample again after 1-2s to confirm it is sustained, not a transient spike.
   - If a child looks hung, note its full command line: `ps -p <child_pid> -o command=`.

3. **Optional stack sample** for a truly frozen process (advanced):
   - macOS: `sample <pid> 3` gives a 3-second native stack sample.
   - Linux: `perf top -p <pid>` or a Python process via `py-spy dump --pid <pid>`.
   - Only grab one if the process is clearly hung and you want to know why. The output is large.

## Report

- PID, CPU%, RSS, state, uptime, command line, child processes.
- Your diagnosis of what is likely wrong, mapped to a row in the table above.
- The confirming evidence (the second CPU sample, the child command line, the state character).
- If every process looks healthy, say that directly. Do not invent a stuck process.

## Notes

- Do not kill or signal any process. Diagnosis only.
- If the user named a specific PID or symptom, focus there first.
- State characters: `R` running, `S` interruptible sleep (normal), `D` uninterruptible sleep (I/O), `T` stopped, `Z` zombie. Only `D`, `T`, `Z`, and sustained-high-`R` are usually pathological.
