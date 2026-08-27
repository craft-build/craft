-- The /tasks picker: the subagents of the focused session, running ones first.
-- The Rust `task` tool spawns them, this file only shows them.
--
-- The host keeps the transcripts, so there is no task state here. Every
-- refresh rebuilds the rows from craft.task.list(), and previewing is just
-- craft.task.focus() with a restore on cancel.

local TextInput = require("craft.text_input")
local ListPicker = require("craft.list_picker")
local Rows = require("picker_rows")

local TITLE = " Tasks "
local FILTER_PREFIX = "\xe2\x9d\xaf " -- "❯ "
local TICK_MS = 100
-- The host does not animate plugin floats, so the picker turns the frame
-- itself: one braille frame per tick, the same set the Rust spinners use.
local RUNNING_FRAMES = {
  "\xe2\xa0\x8b ", -- "⠋ "
  "\xe2\xa0\x99 ", -- "⠙ "
  "\xe2\xa0\xb9 ", -- "⠹ "
  "\xe2\xa0\xb8 ", -- "⠸ "
  "\xe2\xa0\xbc ", -- "⠼ "
  "\xe2\xa0\xb4 ", -- "⠴ "
  "\xe2\xa0\xa6 ", -- "⠦ "
  "\xe2\xa0\xa7 ", -- "⠧ "
  "\xe2\xa0\x87 ", -- "⠇ "
  "\xe2\xa0\x8f ", -- "⠏ "
}
local MAIN_ICON = "\xe2\x97\x8f " -- "● "
-- The footer counts never animate, so they get a real bullet.
local RUNNING_COUNT_ICON = "\xe2\x97\x8f " -- "● "
local DONE_ICON = "\xe2\x9c\x93 " -- "✓ "
local ERROR_ICON = "\xe2\x9c\x97 " -- "✗ "
local NO_MATCHES_HINT = "  No matches"
local FOOTER_KEYS = { { "Enter", "open" }, { "Esc", "cancel" } }
local HINT_KEY = "Ctrl+X"
-- The main chat has no status, so it falls through to MAIN.
local ICONS = {
  done = { DONE_ICON, "success" },
  error = { ERROR_ICON, "error" },
}
local MAIN = { MAIN_ICON, "accent" }

local board = nil

local function dispw(s)
  return utf8.len(s) or #s
end

-- Opening a subagent puts its transcript where the main chat was, and the picker
-- is the only way back, so the input area advertises the key while the session
-- has subagents. craft.task.list() suspends and autocmd callbacks cannot, so the
-- round-trip runs off to the side.
local function refresh_hint()
  craft.async.run(function()
    local n = 0
    for _, task in ipairs(craft.task.list() or {}) do
      -- The main chat is listed too, and it is the one entry without a status.
      if task.status then
        n = n + 1
      end
    end
    if n == 0 then
      craft.ui.set_status_hint(nil)
    else
      craft.ui.set_status_hint({
        { string.format(" %d %s ", n, n == 1 and "task" or "tasks"), "dim" },
        { HINT_KEY, "accent" },
        { " ", "" },
      })
    end
  end)
end

local function icon_of(task)
  if task.status == "working" then
    return RUNNING_FRAMES[(board.frame % #RUNNING_FRAMES) + 1], "accent", true
  end
  local icon = ICONS[task.status] or MAIN
  return icon[1], icon[2], nil
end

-- The counts describe the rows on screen, so a filter that hides half the list
-- has to retally them.
local function update_footer(counts)
  local footer = {}
  if counts.running > 0 then
    footer[#footer + 1] = { RUNNING_COUNT_ICON .. counts.running, "running" }
  end
  if counts.finished > 0 then
    footer[#footer + 1] = { DONE_ICON .. counts.finished, "finished" }
  end
  for _, key in ipairs(FOOTER_KEYS) do
    footer[#footer + 1] = key
  end
  board.win:set_config({ footer = footer })
end

-- The cursor follows its task while the task is still listed, and otherwise
-- falls to whatever row took over the old position.
local function rebuild()
  local previous = Rows.index_of(board.rows, board.sel_id) or 1
  local built = Rows.build(board.tasks, board.input:value())
  board.rows = built.rows
  local idx = Rows.index_of(board.rows, board.sel_id) or math.min(previous, math.max(#board.rows, 1))
  board.sel_id = board.rows[idx] and board.rows[idx].task.id or nil
  update_footer(built.sections)
end

local function render()
  local lines = {}
  local inner = board.width - 4
  board.reserved = ListPicker.render_header(board.win, lines, board.input, FILTER_PREFIX, inner)
  local cursor_line = board.reserved
  local words = ListPicker.split_words(board.input:value())
  for _, row in ipairs(board.rows) do
    if row.section then
      lines[#lines + 1] = { { "  " .. row.section, "keybind_section" } }
    end
    local task = row.task
    local selected = task.id == board.sel_id
    local base = selected and "selected" or "item"
    local icon, icon_style, spinning = icon_of(task)
    if selected then
      icon_style = "selected"
    end
    local line = { { "  ", base }, { icon, icon_style } }
    local match_style = selected and "match_selected" or "match"
    for _, span in ipairs(ListPicker.highlight_spans(task.name, words, base, match_style)) do
      line[#line + 1] = span
    end
    -- Rows with nothing on the right would otherwise end short of the border
    -- and read as padding on one side only, so the bar runs the full width.
    local trail = board.width - 2 - dispw(icon) - dispw(task.name)
    if trail > 0 then
      line[#line + 1] = { string.rep(" ", trail), base }
    end
    lines[#lines + 1] = line
    if selected then
      cursor_line = #lines
    end
  end
  if #board.rows == 0 then
    lines[#lines + 1] = { { NO_MATCHES_HINT, "dim" } }
  end
  board.buf:set_lines(lines)
  board.win:set_cursor(cursor_line)
end

-- The one host round-trip. `list()` suspends this coroutine and the picker can
-- close while it waits, so bail out unless this board is still the current one.
local function refresh()
  local this_board = board
  local tasks, err = craft.task.list()
  if board ~= this_board then
    return
  end
  if err then
    craft.ui.flash(err)
    return
  end
  board.tasks = tasks
  rebuild()
  render()
end

-- The only exit. Unless the user committed, it puts back whatever was on screen
-- before the picker opened, so a cancelled preview never sticks.
local function finish(commit)
  local closing = board
  if not closing then
    return
  end
  board = nil
  if not commit and closing.origin_id then
    craft.task.focus(closing.origin_id)
  end
  closing.win:close()
end

-- Previewing is a real focus, so the transcript behind the float is the one the
-- host already draws. Render first for an instant cursor and let the host catch
-- up next frame.
local function move_sel(delta, wrap)
  local n = #board.rows
  if n == 0 then
    return
  end
  local cur = Rows.index_of(board.rows, board.sel_id) or 1
  local idx
  if wrap then
    idx = (cur - 1 + delta) % n + 1
  else
    idx = math.min(math.max(cur + delta, 1), n)
  end
  board.sel_id = board.rows[idx].task.id
  render()
  local _, err = craft.task.focus(board.sel_id)
  if err then
    craft.ui.flash(err)
  end
end

local function page_size()
  return math.max(board.height - board.reserved - 1, 1)
end

local function open_selected()
  if not board.sel_id then
    return
  end
  local _, err = craft.task.focus(board.sel_id)
  if err then
    craft.ui.flash(err)
    return
  end
  finish(true)
end

local function handle_key(key)
  if key == "ctrl+c" or key == "ctrl+x" then
    finish(false)
  elseif key == "esc" then
    if board.input:is_empty() then
      finish(false)
    else
      board.input:clear()
      rebuild()
      render()
    end
  elseif key == "up" then
    move_sel(-1, true)
  elseif key == "down" then
    move_sel(1, true)
  elseif key == "pageup" then
    move_sel(-page_size())
  elseif key == "pagedown" then
    move_sel(page_size())
  elseif key == "enter" then
    open_selected()
  elseif board.input:handle_key(key) ~= "ignored" then
    rebuild()
    render()
  end
end

local function open()
  if board then
    return
  end
  local buf = craft.ui.buf()
  local win = craft.ui.open_win(buf, {
    title = TITLE,
    width = "70%",
    height = "70%",
    border = "rounded",
    focus = true,
    footer = FOOTER_KEYS,
  })
  board = {
    win = win,
    buf = buf,
    width = win.width,
    height = win.height,
    input = TextInput.new(),
    -- Owned by render(), the only place that knows how tall the query block
    -- ended up once it wrapped.
    reserved = 0,
    frame = 0,
    tasks = {},
    rows = {},
  }
  refresh()
  if not board then
    return
  end
  for _, task in ipairs(board.tasks) do
    if task.focused then
      board.origin_id, board.sel_id = task.id, task.id
    end
  end
  render()

  while board do
    local ev = board.win:recv(TICK_MS)
    if not ev or ev.type == "close" then
      -- The window is already gone, so there is nothing to restore into.
      finish(true)
    elseif ev.type == "timeout" then
      board.frame = board.frame + 1
      if board.expired then
        -- The session changed under us and its ids mean nothing here now.
        finish(true)
      elseif board.dirty then
        board.dirty = false
        refresh()
      elseif board.frame % 2 == 0 then
        render()
      end
    elseif ev.type == "key" then
      handle_key(ev.key)
    elseif ev.type == "paste" then
      board.input:insert_text(ev.text)
      rebuild()
      render()
    elseif ev.type == "resize" then
      board.width = ev.width
      board.height = ev.height
      render()
    end
  end
end

-- Autocmds run synchronously while a refresh needs an async round-trip, so both
-- handlers only raise a flag and let the recv tick do the work.
craft.api.create_autocmd({ "TaskStatusChanged", "SessionStatusChanged" }, {
  callback = function()
    if board then
      board.dirty = true
    end
  end,
})

-- The picker only ever shows the focused session, so a session switch closes it
-- instead of leaving ids from elsewhere on screen.
craft.api.create_autocmd("SessionFocusChanged", {
  callback = function()
    if board then
      board.expired = true
    end
  end,
})

-- Every way the subagent count of the focused session can change.
craft.api.create_autocmd({ "TaskStatusChanged", "SessionFocusChanged", "SessionReset" }, {
  callback = refresh_hint,
})

craft.api.register_command({
  name = "/tasks",
  description = "Browse and search tasks",
  handler = open,
})

craft.keymap.set("n", "<C-x>", open, { desc = "Open tasks" })
