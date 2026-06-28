Commit or discard a pending `ast_edit` proposal by its `edit_id`.

- `action="apply"`: writes every staged file. Each file is written in full, and a backup is pushed to the safety snapshot store so `safety undo` can roll it back. The pending proposal is consumed.
- `action="discard"`: drops the proposal without writing anything. The tree is left untouched.

Pass the `edit_id` returned by the prior `ast_edit` call. If files changed on disk since the proposal was staged, the apply surfaces a stale-read error so you can re-read and re-run `ast_edit`.
