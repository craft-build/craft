local helpers = require("todo_helpers")

-- One Lua runtime serves every session, so todos are keyed by the session
-- that wrote them and the panel only renders for the focused session.
local todos = {}
local focused = nil
local state = {
  win = nil,
  buf = nil,
}

local function items_of(sid)
  return todos[sid or ""] or {}
end

local function is_focused(sid)
  return not focused or sid == focused
end

local function hide_panel()
  if state.win then
    state.win:hide()
  end
  craft.ui.set_status_hint(nil)
end

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

local function update_panel(items)
  ensure_panel()
  helpers.render_todos(state.buf, items)

  local done = 0
  for _, item in ipairs(items) do
    if item.status == "completed" or item.status == "cancelled" then
      done = done + 1
    end
  end
  local total = #items

  local rows = craft.ui.terminal_size().rows
  state.win:set_config({ height = helpers.fit_panel_height(total, rows) })
  craft.ui.set_status_hint({
    { done .. "/" .. total, "dim" },
    { " Ctrl+T", "dim" },
  })
  state.win:show()
end

local function sync_panel(items)
  if #items == 0 then
    hide_panel()
  else
    update_panel(items)
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
  handler = function(input, ctx)
    if not input.todos then
      return "error: todos array is required"
    end

    local sid = (ctx and ctx:session_id()) or ""
    if #input.todos == 0 then
      todos[sid] = nil
      if is_focused(sid) then
        hide_panel()
      end
      return "Todos cleared"
    end

    todos[sid] = input.todos
    if is_focused(sid) then
      update_panel(input.todos)
    end
    return ""
  end,
})

craft.api.create_autocmd("SessionStart", {
  callback = function(ev)
    local sid = ev.data and ev.data.session_id
    if sid then
      todos[sid] = nil
    end
    if is_focused(sid) then
      if state.win then
        state.win:hide()
        state.win = nil
      end
      state.buf = nil
      craft.ui.set_status_hint(nil)
    end
  end,
})

craft.api.create_autocmd({ "TurnEnd", "SessionReset" }, {
  callback = function(ev)
    local sid = ev.data and ev.data.session_id or ""
    todos[sid] = nil
    if is_focused(sid) then
      hide_panel()
    end
  end,
})

craft.api.create_autocmd("SessionFocusChanged", {
  callback = function(ev)
    focused = ev.data and ev.data.session_id
    sync_panel(items_of(focused))
  end,
})
