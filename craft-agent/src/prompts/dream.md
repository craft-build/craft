# Dream: Memory Consolidation

Review your project memory and the recent conversation, then consolidate memory so it stays useful and current. This is a curation pass, not a work pass.

## Steps

1. Use the `memory` tool with `view` (no path) to list all current memory files.
2. Read each file with `memory view <name>`.
3. Decide what to do with each entry:
   - **Merge**: if two entries cover the same topic, combine them into one and delete the redundant copy.
   - **Update**: if an entry is outdated or incomplete based on the recent conversation, rewrite it with current information.
   - **Delete**: if an entry is stale, wrong, or no longer relevant, delete it.
   - **Add**: if the recent conversation surfaced a non-obvious gotcha, decision, or pattern that is NOT yet in memory, add it as a new concise entry.
4. Apply all changes using `memory write` and `memory delete`.

## Rules

- Keep entries concise. Each file should justify its existence.
- Prefer fewer, higher-quality entries over many small ones.
- Do not duplicate information that is obvious from the code or README.
- Do not remove entries that are still relevant, even if old.
- Convert relative dates ("yesterday", "last week") to absolute `YYYY-MM-DD` so entries stay interpretable as time passes.
- If a memory contradicts what the recent conversation revealed, fix or delete it at the source rather than leaving both versions.
- Report a one-paragraph summary of what you consolidated at the end. If nothing changed and memory is already tight, say so explicitly — a no-op is a valid outcome.
