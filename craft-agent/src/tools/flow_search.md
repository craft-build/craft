Search the current Flow workstream's persisted documents (goal, plan, requirements, QA, integration, verification) by semantic relevance to a natural-language query. Returns the top-k matching document paths with similarity scores.

Only available when running inside a Flow stage with `flow.semantic_index` enabled. Use it to recall prior-stage context (e.g. the goal's acceptance criteria, a chunk's requirements) without re-reading every document.

The hits only list paths. To read the actual content of a hit, pass the path to the `read` tool with the `flow://` scheme, e.g. `read("flow://goal.md")`. Use `read("flow://*")` to list every document in the workstream.
