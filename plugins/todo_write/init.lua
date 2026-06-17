local helpers = require("todo_helpers")

local state = {
  items = {},
  win = nil,
  buf = nil,
}

local function ensure_panel()
  if state.buf and state.win then
    return
  end

  state.buf = craft.ui.buf()
  state.win = craft.ui.open_win(state.buf, {
    split = "panel",
    visible = false,
    focus = false,
    height = "30%",
    width = "50%",
    title = "Todos",
    footer = nil,
    footer_content = nil,
    col = nil,
    row = nil,
  })
end

local function update_panel()
  ensure_panel()
  helpers.render_todos(state.buf, state.items)

  local done = 0
  for _, item in ipairs(state.items) do
    if item.status == "completed" or item.status == "cancelled" then
      done = done + 1
    end
  end
  local total = #state.items

  if total > 0 then
    local rows = craft.ui.terminal_size().rows
    state.win:set_config({ height = helpers.fit_panel_height(total, rows) })
    craft.ui.set_status_hint({
      { done .. "/" .. total, "dim" },
      { " Ctrl+T", "dim" },
    })
    state.win:show()
  else
    craft.ui.set_status_hint(nil)
    state.win:hide()
  end
end

craft.api.register_tool({
  name = "todo_write",
  description = "Track and update progress on multi-step tasks. Use this tool to plan and track tasks (must be 3+ steps). Update after EACH completed step, not only all at once. Each task needs an id (e.g. T1, T1.1), content, and status. Parent-child relationships are supported via the parent field.",
  schema = {
    type = "object",
    required = { "todos" },
    properties = {
      todos = {
        type = "array",
        description = "List of tasks to track",
        items = {
          type = "object",
          required = { "id", "content", "status" },
          properties = {
            id = {
              type = "string",
              description = "Hierarchical task id, e.g. T1, T1.1, T2",
            },
            parent = {
              type = "string",
              description = "Parent task id (optional). Use to nest subtasks.",
            },
            content = {
              type = "string",
              description = "Task description",
            },
            status = {
              type = "string",
              description = "pending, in_progress, completed, or cancelled",
            },
            owner = {
              type = "string",
              description = "Subagent name owning this task (optional)",
            },
          },
        },
      },
    },
  },
  handler = function(input)
    if not input.todos then
      return "error: todos array is required"
    end

    if #input.todos == 0 then
      state.items = {}
      if state.win then
        state.win:hide()
      end
      craft.ui.set_status_hint(nil)
      return "Todos cleared"
    end

    state.items = input.todos
    update_panel()
    return ""
  end,
})

craft.api.create_autocmd("SessionStart", {
  callback = function()
    state.items = {}
    if state.win then
      state.win:hide()
      state.win = nil
    end
    if state.buf then
      state.buf = nil
    end
    craft.ui.set_status_hint(nil)
  end,
})
