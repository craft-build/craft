local M = {}

local STATUS_MARKERS = {
  pending = "[ ]",
  in_progress = "[•]",
  completed = "[✓]",
  cancelled = "[x]",
}

local STATUS_STYLES = {
  pending = "todo_pending",
  in_progress = "todo_in_progress",
  completed = "todo_completed",
  cancelled = "todo_cancelled",
}

local BORDER_CHROME = 2
local MAX_HEIGHT_FRACTION = 0.6

M.STATUS_MARKERS = STATUS_MARKERS
M.STATUS_STYLES = STATUS_STYLES

function M.flatten_tree(items)
  local id_map = {}
  for i, item in ipairs(items) do
    id_map[item.id or ("__" .. i)] = i
  end

  local visited = {}
  local out = {}

  local function visit(parent_id, depth)
    for i, item in ipairs(items) do
      if not visited[i] then
        local mine = false
        if parent_id == nil then
          if not item.parent or item.parent == "" or not id_map[item.parent] then
            mine = true
          end
        else
          if item.parent == parent_id then
            mine = true
          end
        end

        if mine then
          visited[i] = true
          table.insert(out, { depth = depth, item = item })

          if item.id and item.id ~= "" then
            visit(item.id, depth + 1)
          end
        end
      end
    end
  end

  visit(nil, 0)

  for i, item in ipairs(items) do
    if not visited[i] then
      table.insert(out, { depth = 0, item = item })
    end
  end

  return out
end

function M.render_todos(buf, items)
  buf:set_lines({})

  local flat = M.flatten_tree(items)
  if #flat == 0 then
    return
  end

  for _, entry in ipairs(flat) do
    local item = entry.item
    local depth = entry.depth
    local indent = string.rep("  ", depth)
    local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS.pending
    local style = STATUS_STYLES[item.status] or "todo_pending"
    local id = ""
    if item.id and item.id ~= "" then
      id = item.id .. " "
    end
    local owner = ""
    if item.owner and item.owner ~= "" then
      owner = " (@" .. item.owner .. ")"
    end
    buf:line({
      { indent .. id .. marker .. " " .. item.content .. owner, style },
    })
  end
end

function M.fit_panel_height(item_count, term_rows)
  local max = math.max(BORDER_CHROME + 1, math.floor(term_rows * MAX_HEIGHT_FRACTION))
  return math.min(item_count + BORDER_CHROME, max)
end

return M
