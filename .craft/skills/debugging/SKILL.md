---
name: debugging
description: Triage a failure from its log or error output, summarize the failure signature, explain the likely cause, and recommend one concrete next step
when_to_use: When the user reports a bug, crash, error, or unexpected behavior and points you at a log file, error output, or asks you to investigate a failure
---

# Debug a failure from its log

Given a symptom and a log, find the failure and explain it. Do not just echo the error line back; explain why it happened and what to do next.

## Steps

1. **Locate the failure signal.** Grep the log for the high-signal patterns:

   ```bash
   grep -nE "\[ERROR\]|\[WARN\]|panic|stack trace|Traceback|FATAL|Exception" <log-file>
   ```

   For a crash, the first stack trace or panic line is usually the root cause; later ones are often downstream noise. For a hang, look at the last lines before the process went silent.

2. **Summarize the failure signature in plain language.** One or two sentences: what the program was doing, what went wrong, where in the code or config. Quote the exact error line, then translate it.

3. **Explain the likely cause, not just the symptom.** The error message describes what happened; the cause is why. Common shapes:
   - A missing or malformed config value, env var, or file path.
   - A version mismatch between a dependency and the runtime.
   - A permissions or ownership error on a file or socket.
   - A resource limit hit (file descriptors, memory, disk).
   - A network or upstream service unreachable.

   If the log alone does not pin the cause, say what is consistent with the evidence and what would confirm it.

4. **Recommend one concrete next step**, not a list of vague possibilities. "Set `DATABASE_URL` in `.env`; the connection string is missing" beats "check your database config." If you are unsure, name the single most likely fix and the command or edit that applies it.

## Defaults

If the user gave no description of the symptom, state what you assumed ("no symptom described, so I treated the first `[ERROR]` as the failure") and proceed. Do not stall waiting for clarification if the log has an obvious failure signal.

If the log is clean, say so. A clean log is a finding: the failure may not be logged, may be in a different file, or may be environmental rather than in-app. Say where to look next.
