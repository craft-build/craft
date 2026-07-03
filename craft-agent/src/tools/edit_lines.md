Edit lines by number. Omit `end` to insert before `start` without removing lines. Set `end` to replace or delete (empty `new_string`) a range.

Since `read` and `grep` show lines as `<nr>: <content>`, this tool works with those line numbers directly. The range is 1-indexed and inclusive on both ends.

- Read the file first to confirm line numbers.
- With `end` omitted: insert `new_string` before `start` (no lines removed). `start` may be one past the last line to append.
- With `end` set: replace lines `start..=end` with `new_string`. Empty `new_string` deletes the range.
- Out-of-range starts or ends are rejected before any write; the file is left unchanged.
- If the original file ended with a trailing newline, the result keeps one; if not, it stays without.
