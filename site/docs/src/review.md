# Review

Craft ships a reviewer workflow on top of the [argosy](./argosy.md) review toolset.

## The reviewer subagent

`/review` — or the `review` tool — snapshots the working-tree diff (or the current state of named paths, with `paths` for a whole-area review of unchanged code) and returns prioritized **P0–P3** findings with a verdict.

The reviewer is read-only: it records findings through `report_finding`, never edits files.

## The session flow

1. `start_review` snapshots the git diff — local tracked changes against a base (`HEAD` by default), exactly one commit, or the current state of named paths (`base "empty"` materializes every tracked line as added).
2. A deterministic pattern pre-pass runs immediately over the added lines when the project's argosy holds styleguide rules with a `pattern` — those findings land first, confidence 1.0.
3. The reviewer agent reads the diff, checks the rules, and reports verified defects with `report_finding`: priority, confidence, affected lines, rule URIs, and a machine-applicable fix when one exists.
4. `review_findings` lists the set (filterable by priority and path) and can export SARIF for GitHub code scanning or reviewdog JSONL for one-click PR comments.

Findings are validated against the snapshot — the path must be a changed file and the line range must fall inside the changed hunks.

## Related commands

- `/scan` — the same machinery aimed at the current state of the codebase.
- `/dream` — speculative/exploratory review mode.
- `/memory` — memory workflow over the argosy.

Findings from review sessions are grounded: rule URIs must resolve to styleguide-rule concepts in the project's argosy (see [Argosy](./argosy.md)).
