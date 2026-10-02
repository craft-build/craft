# Evaluate and restore useful repository navigation

This is conditional capability work in the active `agent/` harness.
Follow the hardening README.

## Problem

The predecessor has dedicated repomap/outline facilities and map controls.
The new harness has native search/read and Argosy integration, so restoring old
navigation wholesale may duplicate capabilities.

## Work

Inventory what is already available, then compare realistic large-repository
navigation tasks against the predecessor's `craft-repomap` and outline workflow.
Identify an unmet need before proposing implementation. Prefer existing
navigation sources and bounded cached summaries over another parallel index.
If no useful gap exists, document that decision instead of adding a subsystem.

## Acceptance

- Produce evidence and a clear restore/defer recommendation.
- Any implemented capability has token/output budgets and stale-cache behavior.
- Tests cover ignored files, large trees and changes between reads.
- Do not use paid model benchmarking without approval.
