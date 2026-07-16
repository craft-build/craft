Edit lines by number. Replaces lines from `start` to `end` (inclusive) with `new_string`. Use empty `new_string` to delete a range.

Since `read` and `grep` show lines as `<nr>: <content>`, this tool works with those line numbers directly. The range is 1-indexed and inclusive on both ends.

- Read the file first to confirm line numbers.
- Replace lines `start..=end` with `new_string`. Empty `new_string` deletes the range.
- To insert lines without removing anything, use the `insert_lines` tool.
- Out-of-range starts or ends are rejected before any write; the file is left unchanged.
- If the original file ended with a trailing newline, the result keeps one; if not, it stays without.
