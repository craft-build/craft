# Review

Use `/review` in the TUI to review your working-tree changes. Craft runs a read-only reviewer and returns prioritized findings with a verdict. This is a code-review workflow, not the [auto-review permission responder](./permissions.md#auto-review-mode).

Findings use priorities from **P0** (most urgent) through **P3** (least urgent).

## The reviewer subagent

The agent can also invoke the `review` tool. It snapshots the working-tree diff by default; its `paths` option can review the current state of named paths, including unchanged code.

The reviewer is read-only: it records findings through `report_finding`, never edits files.

## How a review works

1. `start_review` snapshots the git diff — local tracked changes against a base (`HEAD` by default), exactly one commit, or the current state of named paths (`base "empty"` materializes every tracked line as added).
2. If the project's Argosy knowledge contains styleguide rules with a `pattern`, a deterministic check runs over added lines first. Those findings are recorded with confidence `1.0`.
3. The reviewer agent reads the diff, checks the rules, and reports verified defects with `report_finding`: priority, confidence, affected lines, rule URIs, and a machine-applicable fix when one exists.
4. `review_findings` lists the set (filterable by priority and path) and can export SARIF for GitHub code scanning or reviewdog JSONL for one-click PR comments.

Findings are validated against the snapshot — the path must be a changed file and the line range must fall inside the changed hunks.

## Related commands

- `/scan` — the same machinery aimed at the current state of the codebase.
- `/dream` — speculative/exploratory review mode.
- `/memory` — memory workflow over the argosy.

Findings from review sessions are grounded: rule URIs must resolve to styleguide-rule concepts in the project's argosy (see [Argosy](./argosy.md)).
