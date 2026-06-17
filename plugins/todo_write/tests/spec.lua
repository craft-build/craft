local helpers = require("todo_helpers")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

local function mock_buf()
  local b = { cleared = false, lines = {} }
  function b:set_lines(lines)
    self.cleared = true
    self.lines = lines
  end
  function b:line(spans)
    self.lines[#self.lines + 1] = spans
  end
  return b
end

local function span_text(span)
  return span[1]
end

local function line_text(line)
  local parts = {}
  for _, span in ipairs(line) do
    parts[#parts + 1] = span_text(span)
  end
  return table.concat(parts)
end

case("render_todos_clears_buf_before_drawing", function()
  local buf = mock_buf()
  helpers.render_todos(buf, { { id = "T1", content = "do thing", status = "pending" } })
  assert(buf.cleared, "render_todos must call buf:set_lines to clear before drawing")
  eq(#buf.lines, 1, "should render one line for one item")
end)

case("render_todos_empty_items_clears_and_draws_nothing", function()
  local buf = mock_buf()
  helpers.render_todos(buf, {})
  assert(buf.cleared, "empty input should still clear the buf")
  eq(#buf.lines, 0, "no lines for empty input")
end)

case("render_todos_status_markers_and_styles", function()
  local buf = mock_buf()
  helpers.render_todos(buf, {
    { id = "T1", content = "a", status = "pending" },
    { id = "T2", content = "b", status = "in_progress" },
    { id = "T3", content = "c", status = "completed" },
    { id = "T4", content = "d", status = "cancelled" },
  })
  eq(buf.lines[1][1][2], "todo_pending", "pending -> todo_pending")
  eq(buf.lines[2][1][2], "todo_in_progress", "in_progress -> todo_in_progress")
  eq(buf.lines[3][1][2], "todo_completed", "completed -> todo_completed")
  eq(buf.lines[4][1][2], "todo_cancelled", "cancelled -> todo_cancelled")
  assert(line_text(buf.lines[1]):find("%[ %]"), "pending marker")
  assert(line_text(buf.lines[2]):find("%[•%]"), "in_progress marker")
  assert(line_text(buf.lines[3]):find("%[✓%]"), "completed marker")
  assert(line_text(buf.lines[4]):find("%[x%]"), "cancelled marker")
end)

case("render_todos_unknown_status_falls_back_to_pending", function()
  local buf = mock_buf()
  helpers.render_todos(buf, { { id = "T1", content = "x", status = "bogus" } })
  eq(buf.lines[1][1][2], "todo_pending", "unknown status uses pending style")
  assert(line_text(buf.lines[1]):find("%[ %]"), "unknown status uses pending marker")
end)

case("render_todos_owner_suffix", function()
  local buf = mock_buf()
  helpers.render_todos(buf, {
    { id = "T1", content = "task", status = "pending", owner = "research-agent" },
  })
  assert(line_text(buf.lines[1]):find("@research%-agent"), "owner should be rendered as @suffix")
end)

case("flatten_tree_orders_children_under_parents", function()
  local flat = helpers.flatten_tree({
    { id = "T1", content = "parent", status = "pending" },
    { id = "T2", content = "child", status = "pending", parent = "T1" },
    { id = "T3", content = "top", status = "pending" },
  })
  eq(#flat, 3)
  eq(flat[1].item.id, "T1")
  eq(flat[1].depth, 0)
  eq(flat[2].item.id, "T2")
  eq(flat[2].depth, 1, "child indented one level")
  eq(flat[3].item.id, "T3")
  eq(flat[3].depth, 0)
end)

case("flatten_tree_orphan_parent_lands_at_top_level", function()
  local flat = helpers.flatten_tree({
    { id = "T1", content = "root", status = "pending" },
    { id = "T2", content = "orphan child", status = "pending", parent = "MISSING" },
  })
  eq(flat[1].item.id, "T1", "root comes first in array order")
  eq(flat[2].item.id, "T2", "orphan whose parent is missing follows in array order")
  eq(flat[2].depth, 0, "orphan treated as depth 0, not nested under a missing parent")
end)

case("flatten_tree_empty_returns_empty", function()
  eq(#helpers.flatten_tree({}), 0)
end)

case("fit_panel_height_scales_to_item_count_plus_chrome", function()
  eq(helpers.fit_panel_height(6, 50), 8, "6 items + 2 border lines")
  eq(helpers.fit_panel_height(0, 50), 2, "0 items still reserves chrome")
end)

case("fit_panel_height_caps_at_max_fraction_of_terminal", function()
  eq(helpers.fit_panel_height(100, 50), 30, "floor(50 * 0.6) = 30")
end)

case("fit_panel_height_keeps_one_content_line_on_tiny_terminal", function()
  eq(helpers.fit_panel_height(5, 2), 3, "capped to chrome + 1 content line")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
