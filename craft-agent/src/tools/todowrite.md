Create or update a hierarchical task list to track multi-step work.

**Use after EACH completed step!**

- Send the complete list each time (replace-all semantics).
- Use ONLY for multi-step work (3+ steps).
- Skip for trivial tasks.
- Use hierarchical ids: top-level tasks are `T1`, `T2`, ...; subtasks nest as `T1.1`, `T1.1.2`.
- Set `parent` to the parent task id to nest a task. Omit `parent` for top-level tasks.
