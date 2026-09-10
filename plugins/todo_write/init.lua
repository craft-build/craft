local helpers = require("todo_helpers")

local MAIN_TASK = "main"

-- `todos[session_id][task_id]`: a subagent shares its session id with the
-- parent, only the task id tells them apart.
local todos = {}
-- Sessions that ran a turn since they were loaded. Restores of their
-- transcript are still in flight while the turn runs, so they are stale.
local live = {}
-- Nothing is focused until the first TaskFocusChanged; restores that land
-- before it still file under their own session, so they show up then.
local focused = { session = "", task = MAIN_TASK }
local state = {
  win = nil,
  buf = nil,
}

local function items_of(sid, task)
  return todos[sid] and todos[sid][task] or {}
end

local function hide_panel()
  if state.win then
    state.win:hide()
  end
  craft.ui.set_status_hint(nil)
end

local function update_panel(items)
  if not state.buf or not state.win then
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

-- Derived from the focused list on every change: gone when empty, open
-- otherwise. A hidden window stays open, so a show-on-every-change panel
-- would resurrect a list the turn already cleared.
local function sync_panel()
  local items = items_of(focused.session, focused.task)
  if #items == 0 then
    hide_panel()
  else
    update_panel(items)
  end
end

local function store(sid, task, items)
  todos[sid] = todos[sid] or {}
  todos[sid][task] = items
  if sid == focused.session and task == focused.task then
    sync_panel()
  end
end

craft.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use todo_write for multi-step tasks (3+ steps); update **after EACH step** (done + next in_progress), never batched at the end.",
})

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

  -- A session load replays the transcript in order, so the last call wins
  -- and the panel picks up where the session left off. A rerender (click,
  -- theme change) replays one call that may be long superseded.
  restore = function(input, _output, _is_error, ctx)
    local items = input.todos or {}
    local sid = ctx:session_id() or ""
    if ctx:restore_reason() == "load" and not live[sid] then
      store(sid, ctx:task_id(), items)
    end
    if #items == 0 then
      return nil
    end
    local body = craft.ui.buf()
    helpers.render_todos(body, items)
    return body
  end,

  handler = function(input, ctx)
    if not input.todos then
      return "error: todos array is required"
    end

    local items = input.todos
    if #items == 0 then
      local sid = ctx:session_id() or ""
      local task = ctx:task_id()
      if todos[sid] then
        todos[sid][task] = nil
      end
      if sid == focused.session and task == focused.task then
        hide_panel()
      end
      return "Todos cleared"
    end

    store(ctx:session_id() or "", ctx:task_id(), items)
    return ""
  end,
})

craft.api.create_autocmd("TurnStart", {
  callback = function(ev)
    live[ev.data.session_id] = true
  end,
})

-- Subagents run inside the parent's turn, so its end clears their lists too.
craft.api.create_autocmd({ "TurnEnd", "SessionReset", "SessionEnd" }, {
  callback = function(ev)
    local sid = ev.data and ev.data.session_id or ""
    todos[sid] = nil
    if ev.event ~= "TurnEnd" then
      live[sid] = nil
    end
    if sid == focused.session then
      sync_panel()
    end
  end,
})

-- Fires on a session switch too, so this is the one focus event the panel
-- needs to follow.
craft.api.create_autocmd("TaskFocusChanged", {
  callback = function(ev)
    focused = { session = ev.data.session_id, task = ev.data.id }
    sync_panel()
  end,
})
