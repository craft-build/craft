# Time-Traveling Stream Rules

Craft watches the model's output as it streams, word by word. With Time-Traveling Stream Rules (TTSR), you can write a rule that fires the moment the model emits text matching a pattern you care about, aborts the current turn, and injects a system reminder so the model tries again. Think of it as course-correction that only costs tokens when a rule fires, not on every turn.

TTSR is off by default. Turn it on in your config:

```toml
[agent.ttsr]
enabled = true
```

## Why Use It

Normal system prompts are paid for on every turn, even when the model never makes the mistake you warned about. TTSR rules sit dormant. The model streams freely, and a rule only wakes up when its pattern matches. This keeps your context lean while still catching recurring problems.

A few examples:

- stop the model from committing a secret key the moment it types one out
- block an anti-pattern like `Box::leak` before the turn finishes
- catch forbidden API calls mid-stream

## Writing a Rule

Rules live in `.craft/rules/*.md` files. Each rule is one line prefixed with `rule:`. The simplest form is just a regex pattern:

```markdown
rule: Box::leak
```

When the model emits text matching `Box::leak`, the turn aborts and Craft injects a reminder that says "Avoid emitting text matching: Box::leak".

You can add a human-readable message after a `|`:

```markdown
rule: TODO\( | "leave no TODOs in committed code"
```

And a repeat policy after a second `|`:

```markdown
rule: secret_key | "do not print secrets" | after-gap:3
```

The full format is:

```
rule: <pattern> | "<message>" | <repeat>
```

All parts except the pattern are optional. Quotes around the message are stripped if present.

## Repeat Policies

A rule that fires once would be useless if the model needs the same reminder every time it slips up. Repeat policies control re-firing across turns within a session.

| Policy | Behavior |
|--------|----------|
| `once` | Fires once per session, then stays silent. This is the default. |
| `after-gap:N` | Re-fires only after `N` turns have passed since the last fire. |

Use `once` for one-time nudges. Use `after-gap:N` for habits the model may slip back into, where re-firing every single turn would be too noisy.

Firing memory resets on compaction, so rules suppressed earlier in a long session can fire again after the context is compacted.

## Where Craft Looks for Rules

Rules are discovered from several places, closest scope first. A file name only appears once (the nearest copy wins).

**Project** (walked from the current directory up to the nearest `.git`):

- `.craft/rules/`
- `.agents/rules/`
- `.claude/rules/`
- `.opencode/rules/`

**Global** (your machine):

- `~/.craft/rules/`
- `~/.config/craft/rules/`

The `.claude/` and `.opencode/` locations are supported so rule files written for those tools work here too.

A single `.md` file may contain many `rule:` lines, mixed freely with prose. Only lines starting with `rule:` are parsed; everything else is ignored. Invalid regex patterns are skipped with a warning, so one bad rule does not break the rest.

## How It Works

Each turn, Craft resets a streaming text buffer. As the model emits text, each delta is appended to the buffer and every rule's regex is tested against the accumulated text. This means a pattern can match across many small chunks, not just within a single one.

When a rule fires:

1. the in-flight stream is aborted
2. the rule's message is wrapped in a `<system-reminder>` and injected into the turn
3. the model retries with the reminder present

TTSR watches stream content only. It is distinct from the guardrails that count repeated tool failures and the stagnation watcher that flags a stuck loop.
