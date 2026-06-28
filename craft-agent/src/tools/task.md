Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Subagent types (set via `subagent_type`):
- `research` (default): Read-only tools. For codebase exploration or gathering context.
- `general`: Full tool access. For delegating implementation work.

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. Each invocation starts fresh - inline any needed context into the prompt.
4. Tell it to return concise summaries with file:line refs, not full file contents.

Optional `output_schema`: pass a JSON Schema (object) describing the structured object the subagent must return. When set, the subagent is instructed to end with a JSON object matching the schema; that object is validated and returned to you as structured JSON (not prose). On a validation mismatch the subagent is re-prompted once, then a clean error is surfaced. Use this when you need machine-readable results you can reference by key instead of re-reading prose.

Optional `isolation`: set to `"worktree"` for a general subagent to run inside a fresh linked git worktree, so its file mutations never touch the parent tree and sibling subagents cannot clobber each other. Requires a git repo; otherwise falls back to the parent cwd.
