# Skills

Skills are reusable workflows written as Markdown. The agent loads a skill on demand with the `skill` tool to pick up instructions and steps for a specific task, instead of you re-explaining them every time.

A skill is a directory containing a `SKILL.md` file. Optional YAML frontmatter sets the name and description; the body is Markdown the agent reads.

```
.craft/skills/
└── deploy/
    └── SKILL.md
```

`SKILL.md`:

```markdown
---
name: deploy
description: Deploy this service to staging with the standard checks
---

1. Run `cargo test --all`.
2. Build the container with `docker build -t svc .`.
3. Push and roll out with `kubectl apply -f k8s/`.
```

If you omit the `name` field, the directory name is used.

## Where Craft Looks

Skills are discovered from several places. The first match for a name wins.

**Config dir** (global, your machine):

- `~/.config/craft/skills/`

**Home-relative** (global):

- `~/.craft/skills/`
- `~/.claude/skills/`
- `~/.config/opencode/skills/`
- `~/.agents/skills/`

**Project** (walked from the current directory up to the nearest `.git`):

- `skills/`
- `.claude/skills/`
- `.opencode/skills/`
- `.agents/skills/`

The `.claude/` and `.opencode/` locations are supported so skills written for those tools work here too.

## Using a Skill

The model calls the `skill` tool with a name, and Craft returns that skill's Markdown body. This is a `read`-kind tool: it only adds context, it does not run anything. You can also nudge the model: "use the deploy skill".

## Distilling Skills

Run `/distill` to scan the current conversation for reusable patterns. Craft proposes new `SKILL.md` files and saves them to memory under `skills/` for you to review and move into your skill directories. See the [memory](./plugins.md#memory) plugin for where those land.
