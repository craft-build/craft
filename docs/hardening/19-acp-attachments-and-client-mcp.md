# Restore the required ACP attachment and MCP capabilities

This is conditional editor-compatibility work in the active `agent/` harness.
Follow the hardening README.

## Problem

`agent/src/acp/mod.rs` advertises a text-focused prompt surface and ignores
client-supplied MCP servers. The predecessor supports images, embedded resources
and client server configuration in `~/Projects/craft/craft-acp/src/server.rs`.

## Work

Confirm the desired ACP capability set. Translate supported image/resource
content into model input without silently dropping attachments. Merge client
MCP configuration under an explicit trust/permission policy; do not execute a
client-supplied server just because configuration arrived. Reuse MCP lifecycle,
validation, cancellation and namespacing.

## Acceptance

- Advertised capabilities match tested behavior.
- Image/resource fixtures survive prompt conversion and session replay.
- Client MCP startup, conflict, denial and cleanup paths are covered.
- Unsupported media fails clearly rather than becoming an empty prompt.
