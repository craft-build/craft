# Remove synthetic grace turns correctly on resume

Implement this task in the active `agent/` harness. Follow the hardening README.

## Problem

`agent/src/run/overflow.rs` removes one exact trailing grace user message.
Grace messages followed by assistant responses, or multiple trailing grace
sequences, may remain and be replayed as genuine conversation.
The predecessor handles trailing pairs in
`~/Projects/craft/craft-agent/src/agent/run/compaction.rs`.

## Work

Define cleanup for synthetic grace turns without deleting real user/assistant
content. Prefer reliable provenance when practical; do not depend on broad text
matching. Preserve pending user input and maintain tool-call/result invariants.
Check how failed runs and persisted history interact before porting old logic.

## Acceptance

- Repeated synthetic suffixes and a grace-plus-assistant pair clean up correctly.
- Real conversations containing similar text remain intact.
- Resume and overflow-recovery tests inspect the next request's history.
