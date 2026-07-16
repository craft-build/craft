Insert lines before a given line number. Lines at `line` and below shift down. Existing lines are preserved. Do not use with the batch tool.

Since `read` and `grep` show lines as `<nr>: <content>`, this tool works with those line numbers directly. The line number is 1-indexed.

- Read the file first to confirm line numbers.
- Insert `new_string` before `line`. Existing lines are never removed.
- `line` may be one past the last line to append at the end of the file.
- To replace or delete a range of lines, use the `edit_lines` tool instead.
- Out-of-range line numbers are rejected before any write; the file is left unchanged.
- If the original file ended with a trailing newline, the result keeps one; if not, it stays without.
