# Implementation order

Optimal port order from `comparison.md`: dependencies first, highest leverage-per-effort early, deep subsystems (subagents, Flow, MCP, plugins) last.

## [x] Phase 1 — Foundations (storage, config, small high-value wins)

1. I.1 Path resolution (XDG dirs, data/state/logs/cache split) — everything storage-related builds on this
2. I.3 CraftId (UUIDv7 base58) — needed by sessions
3. C.16 Prompt slot system — templated system prompts, cheap now, used everywhere later
4. C.15 Instructions discovery (AGENTS.md) — big behavior win, no deps
5. A.2 glob tool (S)
6. A.2 list tool (S)
7. A.3 multiedit (S)
8. A.3 inspect (S)
9. A.5 todo_write (S)
10. A.5 list_tools (S)
11. B.6 Tool dedup cache (S)
12. D.1 Token estimation & calibration (S)
13. D.7 Recency tail (S)
14. I.7 Misc state (input history, model/theme state, logs, atomic IO)
15. G.8 Shell completions (S)

## Phase 2 — Context management & reliability

16. B.5 Tool-output pre-compression — highest token-savings-per-effort
17. D.2 Read lifecycle (stale/superseded read marking) — cheapest big context win
18. D.9 Carry-protection & compaction buffers, proactive thresholds
19. D.8 Targeted LLM compaction + overflow retry ladder
20. D.5 Reversible compression store + retrieve tool (S)
21. C.4 Stalled-turn nudging (S)
22. C.3 MaxTokens continuation (verify Rig coverage)
23. C.9 Per-tool guardrails
24. E.1 Snapshots & rollback / `/undo` — also unlocks safety for bash later

## Phase 3 — Permissions & bash

25. B.1 Permission rule engine — backbone for bash/web/task
26. B.2 Compound-command permission parsing (tree-sitter bash)
27. A.1 bash tool (streaming, background tasks, timeouts)
28. E.9 Process safety (ChildGuard)
29. B.9 In-place-edit detection for bash
30. B.10 Untrusted-output wrapping (S)
31. A.1 apply_patch (L)
32. A.1 batch (M)
33. A.3 fuzzy_replace (M)
34. A.3 move_file (M)
35. A.3 recursive/anchored delete (delta)
36. B.8 Parallel dispatch with write-conflict barrier
37. B.3 Bash sandbox (macOS Seatbelt + Linux landlock) — can slip later if permission engine is solid
38. E.7 LLM auto-review of permissions

## Phase 4 — Agent loop hardening

39. C.1 Rich agent event stream — substrate for TUI features below
40. C.6 Cancellation architecture (CancelToken/CancelMap, history sanitization)
41. C.5 Context-overflow recovery
42. C.2 Streaming retry state machine (key rotation, model-chain fallback, clamping)
43. C.8 Doom-loop tracker
44. H.11 Timeout/retry policy (verify Rig coverage)
45. E.5 JSON repair pipeline (S)
46. E.10 Auth-error reauth wait (S)

## Phase 5 — Providers, usage & sessions

47. H.6 Usage & cost accounting — prerequisite for /usage, /stats, subagent cost chaining
48. H.3 Model registry & tiers (weak/medium/strong/compaction)
49. H.2 models.dev catalog with disk cache
50. I.6 Cost ledger (S)
51. I.2 Session persistence (JSONL) — prerequisite for resume, rewind, /sessions
52. I.5 Plans storage (S)
53. C.14 Headless session API
54. C.20 Cost ledger wiring to /usage, /stats

## Phase 6 — Web & media

55. E.8 SSRF protection on webfetch
56. A.4 webfetch
57. A.4 websearch
58. A.4 view_image (needs F.13 image pipeline — schedule with Phase 8 if not ready)

## Phase 7 — TUI core

59. F.1 Dirty-flag repaint / render worker
60. F.6 Markdown rendering engine (stateful streaming re-wrap)
61. F.6 Syntax highlighting (syntect)
62. F.6 Diff rendering + collapsible diff cards
63. F.6 Scrollback engine
64. F.6 Tool display specialization
65. F.2 Composer upgrades (history, word motions, $EDITOR)
66. F.2 Ctrl-C tri-state (S)
67. F.2 Suspend Ctrl-Z (S)
68. F.6 OSC-8 hyperlinks (S)
69. F.7 Status bar deltas, notifications, thinking indicators, splash (all S)

## Phase 8 — TUI features & modes

70. F.2 Mode cycling Tab + C.17 Plan mode (skip Flow for now)
71. F.2 Bash bang-mode (`!` / `!!`)
72. F.5 Permission prompt overlay (scope negotiation)
73. F.5 Model picker with tier assignment
74. F.5 Usage & stats modals
75. F.5 Theme picker
76. F.3 Fuzzy message search (Ctrl-F)
77. F.3 File picker (Ctrl-S)
78. F.3 Session resume / picker + background checkpointing
79. F.3 Todo/plan panel (Ctrl-T) + plan editor handoff (Ctrl-O)
80. F.1 Data-driven keybindings + help modal
81. F.6 Inline images (kitty protocol)
82. A.5 question tool (needs UI + ACP elicitation)

## Phase 9 — CLI, headless & protocols

83. G.1 Core CLI flags
84. G.3 Print mode (headless, text + stream-json)
85. G.5 ACP server extensions (permissions, elicitation, MCP passthrough, resume)
86. G.2 Subcommands (models, stats, doctor, prompt, term)
87. G.6 Setup & first-run flow
88. G.7 Self-update & rollback (pin release digest)
89. G.9 Shell integration (`craft term`)

## Phase 10 — Extensibility & deep subsystems

90. J.4 Skills (discovery + skill tool + skill:// reads)
91. J.5 Custom commands & recipes
92. A.5 skill tool (if not done with J.4)
93. A.5 sessions (model-side)
94. B.11 MCP client
95. A.5 task (subagents) — single-tier first, then tiers/isolation/output_schema
96. F.3 Subagent task chats
97. C.12 Advisor (post-turn review)
98. D.5 auto-retrieve (semantic, needs embeddings)
99. C.17 Flow mode + J.6 Flow workstreams (XL — last)
