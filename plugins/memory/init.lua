local ToolView = require("craft.tool_view")
local helpers = require("memory_helpers")
local ListPicker = require("craft.list_picker")

local WRITE_TOOLS = { "write", "edit", "multiedit", "edit_lines", "insert_lines" }

local function memories_path_suffix()
  local cwd = craft.uv.cwd()
  local root = craft.fs.root(cwd, ".git") or cwd
  return "projects/" .. helpers.project_id(root) .. "/memories"
end

local function legacy_dir_if_exists(suffix)
  local legacy = craft.env.legacy_dir()
  if not legacy then
    return nil
  end
  local dir = craft.fs.joinpath(legacy, suffix)
  local meta = craft.fs.metadata(dir)
  if meta and meta.is_dir then
    return dir
  end
end

-- Notes live outside cwd, where file-write tools normally prompt; pre-allow
-- them here so the agent can edit notes directly. Reads may come from the
-- legacy dir while writes go to the state dir, so cover both.
local function register_write_rules()
  local suffix = memories_path_suffix()
  local dirs = { legacy_dir_if_exists(suffix) }
  local state = craft.env.state_dir()
  if state then
    dirs[#dirs + 1] = craft.fs.joinpath(state, suffix)
  end
  for _, dir in ipairs(dirs) do
    for _, tool in ipairs(WRITE_TOOLS) do
      -- The edit sub-tools are opt-in, and a rule naming an unregistered tool
      -- is dropped with a warning. Ask first, or a default config logs that
      -- warning at every startup.
      if craft.api.has_tool(tool) then
        craft.api.register_permission_rule({ tool = tool, scope = dir .. "/**" })
      end
    end
  end
end
register_write_rules()

local function resolve_dir(check_legacy)
  local suffix = memories_path_suffix()
  if check_legacy then
    local dir = legacy_dir_if_exists(suffix)
    if dir then
      return dir
    end
  end
  local state = craft.env.state_dir()
  if not state then
    return nil, "cannot resolve state dir"
  end
  return craft.fs.joinpath(state, suffix)
end

craft.api.register_prompt_hint({
  prompt = "system",
  slot = "after_instructions",
  content = function()
    local dir = resolve_dir(true)
    if not dir then
      return nil
    end
    local entries = helpers.collect_file_entries(dir)
    if #entries == 0 then
      return nil
    end
    table.sort(entries, function(a, b)
      return a[1] < b[1]
    end)
    local header = "\n\nMemory files (use the memory tool to view/update):\n"
    local out = header
    local shown = 0
    local truncated = false
    for _, e in ipairs(entries) do
      local line = "- " .. e[1] .. " (" .. e[2] .. " bytes)\n"
      if shown >= helpers.MAX_HINT_FILES or #out + #line - #header > helpers.MAX_HINT_BYTES then
        truncated = true
        break
      end
      out = out .. line
      shown = shown + 1
    end
    if truncated then
      out = out .. "- ... (" .. (#entries - shown) .. " more; use the memory tool to view)\n"
    end
    return out
  end,
})

craft.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Proactively save non-obvious project gotchas and architecture decisions to **memory**.",
})

craft.api.register_prompt_hint({
  prompt = "system",
  slot = "after_instructions",
  content = function()
    local dir = resolve_dir(true)
    if not dir then
      return nil
    end
    local fp = helpers.safe_resolve(dir, "checkpoint.md")
    if not fp then
      return nil
    end
    local meta = craft.fs.metadata(fp)
    if not meta then
      return nil
    end
    local content = craft.fs.read(fp)
    if not content or content == "" then
      return nil
    end
    return "\n\n## Checkpoint (from last session)\n\n" .. content .. "\n"
  end,
})

local function render_content(content, path, ctx)
  local buf = craft.ui.buf()
  local tol = ctx:tool_output_lines()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.other) or 20,
    keep = "head",
  })
  buf:on("click", function()
    view:toggle()
  end)

  local ext = path:match("%.([^%.]+)$") or "md"
  if not view:set_highlight(content, ext) then
    view:append_text(content)
  end
  view:finish()
  return buf
end

local function semantic_view(query, dir, ctx)
  local results, err = helpers.semantic_search(dir, query)
  if not results then
    return nil, err or "no matching memories"
  end
  if #results == 0 then
    return nil, "no memories matching '" .. query .. "'"
  end
  local lines = { "Semantic search results for '" .. query .. "':\n" }
  local rendered = {}
  for _, entry in ipairs(results) do
    local filename = entry[1]
    local sim = entry[2]
    lines[#lines + 1] = "## " .. filename .. " (similarity: " .. string.format("%.2f", sim) .. ")"
    local fp = helpers.safe_resolve(dir, filename)
    if fp then
      local content = craft.fs.read(fp)
      if content then
        lines[#lines + 1] = content
        lines[#lines + 1] = ""
        rendered[#rendered + 1] = content
      end
    end
  end
  local output = table.concat(lines, "\n")
  return {
    llm_output = output,
    body = render_content(table.concat(rendered, "\n---\n"), "search.md", ctx),
  }
end

local function lexical_view(query, dir, ctx)
  local results = helpers.keyword_search(dir, query)
  if not results or #results == 0 then
    return nil
  end
  local lines = { "Keyword search results for '" .. query .. "':\n" }
  local rendered = {}
  for _, entry in ipairs(results) do
    local filename = entry[1]
    local score = entry[2]
    lines[#lines + 1] = "## " .. filename .. " (score: " .. string.format("%.2f", score) .. ")"
    local fp = helpers.safe_resolve(dir, filename)
    if fp then
      local content = craft.fs.read(fp)
      if content then
        lines[#lines + 1] = content
        lines[#lines + 1] = ""
        rendered[#rendered + 1] = content
      end
    end
  end
  local output = table.concat(lines, "\n")
  return {
    llm_output = output,
    body = render_content(table.concat(rendered, "\n---\n"), "search.md", ctx),
  }
end

local function cmd_view(path, dir, ctx)
  if not path then
    helpers.cleanup_vectors(dir)
    return helpers.list_memories(dir)
  end
  local file_path, err = helpers.safe_resolve(dir, path)
  if file_path then
    local content, read_err = craft.fs.read(file_path)
    if content then
      return {
        llm_output = content,
        body = render_content(content, path, ctx),
      }
    end
    return nil, "read error: " .. tostring(read_err)
  end
  local lexical = lexical_view(path, dir, ctx)
  if lexical then
    return lexical
  end
  if helpers.has_embed() then
    return semantic_view(path, dir, ctx)
  end
  return nil, "'" .. path .. "' not found"
end

local function cmd_write(path, content, dir, ctx)
  local lc = helpers.count_lines(content)
  if lc > helpers.MAX_LINES_PER_FILE then
    return nil, "content exceeds " .. helpers.MAX_LINES_PER_FILE .. " lines (" .. lc .. " lines); reduce content size"
  end
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  local meta = craft.fs.metadata(file_path)
  local existing_size = meta and meta.size or 0
  if helpers.dir_total_bytes(dir) - existing_size + #content > helpers.MAX_DIR_BYTES then
    return nil, "memory directory would exceed " .. helpers.MAX_DIR_BYTES .. " byte limit; delete stale entries first"
  end
  craft.fs.mkdir(dir, { parents = true })
  local ok, write_err = craft.fs.write(file_path, content)
  if not ok then
    return nil, "write error: " .. tostring(write_err)
  end
  helpers.store_embedding(dir, path, content)
  return {
    llm_output = "wrote " .. path .. " (" .. lc .. " lines)",
    body = render_content(content, path, ctx),
  }
end

local function cmd_delete(path, dir)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  if not craft.fs.metadata(file_path) then
    return nil, "'" .. path .. "' does not exist"
  end
  local ok, rm_err = craft.fs.rm(file_path)
  if not ok then
    return nil, "delete error: " .. tostring(rm_err)
  end
  helpers.remove_embedding(dir, path)
  return "deleted " .. path
end

local function with_dir(res, dir)
  local prefix = "dir: " .. dir .. "\n\n"
  if type(res) == "string" then
    return prefix .. res
  end
  res.llm_output = prefix .. res.llm_output
  return res
end

craft.api.register_tool({
  name = "memory",
  description = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.\n\n"
    .. "- Save important context before compaction or to build up project knowledge.\n"
    .. "- Keep entries concise and current. Delete outdated information.\n"
    .. "- Use `view` with a search query (not a filename) to recall memories: keyword search is always-on; semantic search is used when available.\n"
    .. "- Notes are plain files; `view` reports the dir, so use the edit tool on `<dir>/<name>` for targeted changes.",

  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "Command: view, write, delete", required = true },
      path = { type = "string", description = "Relative path (e.g. 'architecture.md'). Omit to list all." },
      content = { type = "string", description = "File content for 'write'" },
    },
  },

  header = function(input)
    if input.path then
      return (input.command or "") .. " " .. input.path
    end
    return input.command
  end,

  restore = function(input, output, _is_error, ctx)
    local content = (input.command == "write" and input.content) or output
    return render_content(content, input.path or "file.md", ctx)
  end,

  handler = function(input, ctx)
    local cmd = input.command
    local dir, dir_err = resolve_dir(cmd == "view")
    if not dir then
      return { llm_output = "error: " .. dir_err, is_error = true }
    end

    local result, err
    if cmd == "view" then
      result, err = cmd_view(input.path, dir, ctx)
    elseif cmd == "write" then
      if not input.path then
        return { llm_output = "error: 'path' is required for write", is_error = true }
      end
      if not input.content then
        return { llm_output = "error: 'content' is required for write", is_error = true }
      end
      result, err = cmd_write(input.path, input.content, dir, ctx)
    elseif cmd == "delete" then
      if not input.path then
        return { llm_output = "error: 'path' is required for delete", is_error = true }
      end
      result, err = cmd_delete(input.path, dir)
    else
      return {
        llm_output = "error: unknown command '" .. tostring(cmd) .. "'. Valid commands: view, write, delete",
        is_error = true,
      }
    end
    if err then
      return { llm_output = "error: " .. err, is_error = true }
    end
    if cmd == "view" then
      return with_dir(result, dir)
    end
    return result
  end,
})

craft.api.register_command({
  name = "/memory",
  description = "View, edit, and delete memory files",
  handler = function()
    local dir = resolve_dir(true)
    if not dir then
      craft.ui.flash("Cannot resolve memory directory")
      return
    end

    local entries = helpers.collect_file_entries(dir)
    if #entries == 0 then
      craft.ui.flash("No memory files yet")
      return
    end
    table.sort(entries, function(a, b)
      return a[1] < b[1]
    end)

    local function build_items()
      local items = {}
      for _, e in ipairs(entries) do
        items[#items + 1] = { label = e[1], detail = "(" .. e[2] .. " bytes)" }
      end
      return items
    end

    local last_cursor = 1
    while true do
      local event = ListPicker.open(build_items(), {
        title = " Memory Files ",
        cursor = last_cursor,
        submit_keys = { "ctrl+o" },
        footer = {
          { "Enter", "open" },
          { "Ctrl+O", "edit" },
          { "Ctrl+D", "delete" },
        },
      })

      if event.type == "close" then
        break
      end

      last_cursor = event.index
      if event.type == "choice" then
        local item = entries[event.index]
        if item then
          local path = craft.fs.joinpath(dir, item[1])
          local code = craft.ui.open_editor(path)
          if code == 0 then
            local meta = craft.fs.metadata(path)
            if meta then
              item[2] = meta.size
            end
          end
        end
      elseif event.type == "delete" then
        local item = entries[event.index]
        local ok, err = craft.fs.rm(craft.fs.joinpath(dir, item[1]))
        if ok then
          helpers.remove_embedding(dir, item[1])
          craft.ui.flash("Deleted " .. item[1])
          table.remove(entries, event.index)
          if #entries == 0 then
            break
          end
          if last_cursor > #entries then
            last_cursor = #entries
          end
        else
          craft.ui.flash("Delete failed: " .. tostring(err))
        end
      else
        break
      end
    end
  end,
})
