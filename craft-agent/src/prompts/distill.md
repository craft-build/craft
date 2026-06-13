# Distill: Skill Discovery

Review the recent conversation and identify reusable workflows that could be packaged as skills. This is an extraction pass.

## Steps

1. Scan the conversation for repeated tool-call patterns or multi-step procedures you performed more than once.
2. For each candidate, evaluate: is this a generalizable workflow that would be useful in future sessions? Is it non-obvious enough to warrant a skill?
3. For high-confidence candidates, draft a `SKILL.md` with:
   - YAML frontmatter: `name`, `description`, `when_to_use`.
   - Markdown body: step-by-step instructions a future agent could follow.
4. Use `memory write skills/<name>.md` to save each proposed skill draft for the user to review.

## Rules

- Only propose skills for workflows that are genuinely reusable and non-trivial.
- Skip one-off tasks or things already covered by existing tools.
- Keep each skill focused on one workflow.
- If nothing is worth distilling, say so and do nothing.
