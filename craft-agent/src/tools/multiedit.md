Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

- Read the file first to get exact content.
- old_string must match the file contents exactly, including all whitespace and indentation.
- Each edit must match exactly once unless replace_all is true. Use replace_all for renaming across a file.
- Edits are applied in sequence - each operates on the result of the previous.
- If any edit fails, none are written.
- Ensure earlier edits don't affect text that later edits need to find.
- Optional per-edit `line_anchor_hash` (12-char hex): when set, the matched lines are hashed and compared before applying; a mismatch rejects the edit with the current content so you can retry. See the `edit` tool for hash details.
