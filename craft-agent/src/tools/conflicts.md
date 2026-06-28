Find and resolve git merge conflicts in the project.

Scans all tracked files for conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`).
Returns each conflicting file with marker locations and branch names.

Resolve conflicts by passing `resolve`:
- `@theirs`: keep the incoming (their branch) side of each conflict.
- `@ours`: keep the current (our branch) side of each conflict.
- `@base`: drop both sides (use when both changes should be removed).

Omit `resolve` to list conflicts only. Use `index` (1-indexed) to resolve a
single conflict within each file; omit it to resolve all conflicts in scope.
