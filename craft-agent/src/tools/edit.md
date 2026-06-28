Replace an exact string match in a file.

- The old_string must appear exactly once unless replace_all is true or occurrence is specified.
- Read the file first to get exact content.
- When copying text from read output, do NOT include the line number prefix (e.g. `42: `) - only the content after it.
- Prefer this over write for targeted changes - it uses far fewer tokens.
- Use replace_all for renaming across a file.
- Use occurrence to select among multiple matches (1-indexed). If old_string matches multiple locations and occurrence is not set, the edit fails with a disambiguation error.
- Fuzzy matching is applied automatically: trailing whitespace, indentation, and Unicode differences are tolerated. The output notes which fuzzy pass matched.
- Optional `line_anchor_hash`: a 12-char hex content-hash of the target line(s). When set, the applier verifies the hash against the current matched lines before writing; a stale anchor is rejected before any write and the current content is returned so you can retry in one step. This avoids corrupting files when your view of the file is stale, and can save output tokens (you point at a hash instead of retyping the whole region). Hash the trim-normalized target lines (blank lines ignored).
