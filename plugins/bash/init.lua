local truncate = require("craft.truncate")
local ToolView = require("craft.tool_view")

local RTK_REWRITE_TIMEOUT_MS = 2000
local RTK_UNSUPPORTED_FLAGS = {
  " -o ",
  " -not ",
  " ! ",
  " -exec ",
  " -execdir ",
  " -print0",
  " -delete",
  " -ok ",
  " -okdir ",
  " -fprint",
  " -fls ",
  " -fprintf ",
}
local SEPARATOR = "──────"

local rtk_available

local bg_jobs = {}
local bg_next_id = 1

local function shell_quote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

local function unquote(s)
  local q = s:sub(1, 1)
  if (q == '"' or q == "'") and s:sub(-1) == q then
    return s:sub(2, -2)
  end
  return s
end

local function parse_cd_hint(input)
  if input.workdir then
    return input.command, input.workdir
  end
  local rest = input.command:match("^cd%s+(.+)$")
  if rest then
    local dir, tail = rest:match("^(.-)%s+&&%s+(.+)$")
    if dir and dir ~= "" then
      return tail, unquote(dir)
    end
  end
  return input.command, nil
end

local function relative_path(p)
  local cwd = craft.uv.cwd()
  if cwd and p:sub(1, #cwd + 1) == cwd .. "/" then
    local rel = p:sub(#cwd + 2)
    return rel == "" and "." or rel
  end
  if cwd and p == cwd then
    return "."
  end
  local home = craft.uv.os_homedir()
  if home and p:sub(1, #home + 1) == home .. "/" then
    local rel = p:sub(#home + 2)
    return rel == "" and "~" or "~/" .. rel
  end
  return p
end

local function build_header_lines(command)
  local header = {}
  local highlighted = craft.ui.highlight(command, "bash")
  if highlighted then
    for _, line in ipairs(highlighted) do
      header[#header + 1] = line
    end
  else
    header[#header + 1] = command
  end
  header[#header + 1] = { { SEPARATOR, "dim" } }
  return header
end

local function rtk_find_unsupported(cmd)
  if not cmd:match("^rtk find ") then
    return false
  end
  for _, flag in ipairs(RTK_UNSUPPORTED_FLAGS) do
    if cmd:find(flag, 1, true) then
      return true
    end
  end
  return false
end

local function rtk_rewrite(command, ctx)
  local config = ctx:config()
  if config and config.no_rtk then
    return nil
  end

  if rtk_available == nil then
    local id = craft.fn.jobstart("rtk --version")
    local result = craft.fn.jobwait(id, RTK_REWRITE_TIMEOUT_MS)
    if result then
      rtk_available = (result.exit_code == 0)
    else
      craft.fn.jobstop(id)
      rtk_available = false
    end
  end

  if not rtk_available then
    return nil
  end

  local cmd = command:match("^%s*(.-)%s*$")
  if cmd:match("^cargo ") and cmd:find(" -- ", 1, true) then
    return nil
  end

  local id = craft.fn.jobstart("rtk rewrite " .. shell_quote(command))
  local result = craft.fn.jobwait(id, RTK_REWRITE_TIMEOUT_MS)
  if not result then
    craft.fn.jobstop(id)
    return nil
  end

  if result.exit_code ~= 0 and result.exit_code ~= 3 then
    return nil
  end

  local rewritten = (result.stdout or ""):match("^%s*(.-)%s*$")
  if rewritten == "" or rewritten == command:match("^%s*(.-)%s*$") then
    return nil
  end
  if rtk_find_unsupported(rewritten) then
    return nil
  end
  return rewritten
end

local function strip_ansi(s)
  return s:gsub("\27%[[%d;]*m", "")
end

local function compress_output(s)
  s = strip_ansi(s)
  local lines = {}
  local prev_blank = false
  for line in (s .. "\n"):gmatch("([^\n]*)\n") do
    if line:match("^%s*$") then
      if not prev_blank then
        lines[#lines + 1] = ""
        prev_blank = true
      end
    else
      lines[#lines + 1] = line
      prev_blank = false
    end
  end
  return table.concat(lines, "\n")
end

local function append_line(output, line)
  if #output > 0 then
    output[#output + 1] = "\n"
  end
  output[#output + 1] = line
end

local function create_bash_view(command, ctx)
  local tol = ctx:tool_output_lines()
  local buf = craft.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.bash) or 5,
    keep = "tail",
  })
  view:set_header(build_header_lines(command))
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local cwd = craft.uv.cwd() or "."

local COMPLEX_TYPES = {
  command_substitution = true,
  process_substitution = true,
  subshell = true,
  arithmetic_expansion = true,
}

local function is_complex(node)
  if COMPLEX_TYPES[node:type()] then
    return true
  end
  for child in node:iter_children() do
    if is_complex(child) then
      return true
    end
  end
  return false
end

local LEAF_COMMAND_TYPES = {
  command = true,
  redirected_statement = true,
  negated_command = true,
  subshell = true,
  compound_statement = true,
  if_statement = true,
  while_statement = true,
  for_statement = true,
  case_statement = true,
  function_definition = true,
  c_style_for_statement = true,
}

local function collect_commands(node, source)
  local out = {}
  local kind = node:type()
  if kind == "program" or kind == "list" then
    for child in node:iter_children() do
      local nested = collect_commands(child, source)
      for _, cmd in ipairs(nested) do
        out[#out + 1] = cmd
      end
    end
  elseif kind == "pipeline" then
    for child in node:iter_children() do
      if child:named() then
        local text = craft.treesitter.get_node_text(child, source):match("^%s*(.-)%s*$")
        if text ~= "" then
          out[#out + 1] = text
        end
      end
    end
  elseif LEAF_COMMAND_TYPES[kind] then
    local text = craft.treesitter.get_node_text(node, source):match("^%s*(.-)%s*$")
    if text ~= "" then
      out[#out + 1] = text
    end
  end
  return out
end

local bash_description = [[Execute a bash command.
Commands run in ]] .. cwd .. [[ by default.

- **DO NOT** use for file ops! Only git, builds, tests, and system commands.
- Use `workdir` param instead of `cd <dir> && <cmd>` patterns.
- Do NOT use to communicate text to the user.
- Chain dependent commands with `&&`. Use batch for independent ones.
- Provide a short `description` (3-5 words).
- Output truncated beyond 2000 lines or 50KB.
- Interactive commands (sudo, ssh prompts) fail immediately.
- Set `background=true` for long-running commands. Returns a `task_id` for use with bash_status, bash_watch, bash_kill.]]

craft.api.register_tool({
  name = "bash",
  kind = "execute",
  description = bash_description,
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The bash command to execute", required = true },
      timeout = { type = "integer", description = "Timeout in seconds (default 120)" },
      workdir = { type = "string", description = "Working directory (default: cwd)" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
      background = { type = "boolean", description = "Run in background, return task_id for later polling" },
    },
  },
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end

    local parser = craft.treesitter.get_parser(command, "bash")
    if not parser then
      return { scopes = { command }, force_prompt = true }
    end

    local root = parser:parse()[1]:root()
    if root:has_error() or is_complex(root) then
      return { scopes = { command }, force_prompt = true }
    end

    local segments = collect_commands(root, command)
    if #segments == 0 then
      segments = { command }
    end
    return { scopes = segments, force_prompt = false }
  end,

  header = function(input)
    local command, workdir = parse_cd_hint(input)
    local s = input.description or command
    if input.background then
      s = s .. " (bg)"
    end
    if workdir then
      s = s .. " in " .. relative_path(workdir)
    end
    if input.timeout then
      local buf = craft.ui.buf()
      buf:line({ { s }, { " (" .. craft.ui.humantime(input.timeout) .. " timeout)", "dim" } })
      return buf
    end
    return s
  end,

  restore = function(input, output, is_error, ctx)
    local command = input.command
    local buf, view = create_bash_view(command, ctx)
    local timeout_secs = output:match("^tool bash timed out after (%d+)s$")
    if timeout_secs then
      view:append({ { "Timed out after " .. timeout_secs .. "s", "dim" } })
    elseif output:match("^Background task:") then
      view:clear()
      for line in (output .. "\n"):gmatch("([^\n]*)\n") do
        if line ~= "" then
          view:append({ { line, "dim" } })
        end
      end
      view:finish()
    elseif is_error then
      local body, code = output:match("^(.-)\nExit code: (%d+)$")
      if body then
        for line in (body .. "\n"):gmatch("([^\n]*)\n") do
          view:append(line)
        end
        view:append({ { "Exit code: " .. code, "dim" } })
      else
        for line in (output .. "\n"):gmatch("([^\n]*)\n") do
          view:append(line)
        end
      end
    else
      if output == "Exit code: 0" or output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      else
        for line in (output .. "\n"):gmatch("([^\n]*)\n") do
          view:append(line)
        end
      end
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end

    local command, workdir = parse_cd_hint(input)
    local config = ctx:config()
    local timeout_secs = input.timeout or (config and config.bash_timeout_secs) or 120
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    if not input.background then
      ctx:set_deadline(timeout_secs)
    end

    local rewritten = rtk_rewrite(command, ctx)
    if rewritten then
      command = rewritten
    end

    local buf, view = create_bash_view(command, ctx)
    local output_parts = {}
    local has_output = false
    local bg_task_id = input.background and ("bg_" .. bg_next_id) or nil
    if bg_task_id then
      bg_next_id = bg_next_id + 1
    end

    local function finish(exit_code)
      local output = table.concat(output_parts)
      output = compress_output(output)
      output = truncate(output, max_lines, max_bytes)

      local is_error = exit_code ~= 0
      local llm_output
      if exit_code == 0 then
        llm_output = output == "" and "Exit code: 0" or output
      else
        if output == "" then
          llm_output = "Exit code: " .. exit_code
        else
          llm_output = output .. "\nExit code: " .. exit_code
        end
      end

      if output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      end

      if is_error then
        view:append({ { "Exit code: " .. exit_code, "dim" } })
      end
      view:finish()

      ctx:finish({ llm_output = llm_output, is_error = is_error, body = buf })
    end

    local job_opts = {
      cwd = workdir,
      env = { GIT_TERMINAL_PROMPT = "0" },
      sandbox = true,
      background = input.background or false,
      on_stdout = function(_, line)
        if not has_output then
          has_output = true
          view:clear()
        end
        append_line(output_parts, line)
        view:append(line)
      end,
      on_stderr = function(_, line)
        if not has_output then
          has_output = true
          view:clear()
        end
        append_line(output_parts, line)
        view:append(line)
      end,
      on_exit = function(_, code)
        if bg_task_id then
          bg_jobs[bg_task_id].status = "exited"
          bg_jobs[bg_task_id].exit_code = code
          return
        end
        finish(code)
      end,
    }
    local job_id = craft.fn.jobstart(command, job_opts)

    if bg_task_id then
      bg_jobs[bg_task_id] = {
        job_id = job_id,
        command = command,
        status = "running",
        exit_code = nil,
        output_parts = output_parts,
      }

      view:append({ { "Background task: " .. bg_task_id, "dim" } })
      view:finish()

      local llm_output = "Background task: "
        .. bg_task_id
        .. "\n"
        .. 'use bash_status(task_id="'
        .. bg_task_id
        .. '") to check output\n'
        .. 'use bash_kill(task_id="'
        .. bg_task_id
        .. '") to terminate'

      return { llm_output = llm_output, is_error = false, body = buf }
    end

    view:append({ { "Waiting for output...", "dim" } })
    return nil
  end,
})

-- bash_status: poll a background task
craft.api.register_tool({
  name = "bash_status",
  kind = "execute",
  description = "Check status and current output of a background bash task.",
  schema = {
    type = "object",
    properties = {
      task_id = { type = "string", description = "The task_id returned by bash", required = true },
    },
  },
  permission_scopes = function()
    return { scopes = { "bash_status" }, force_prompt = false }
  end,
  header = function(input)
    return "status " .. (input.task_id or "?")
  end,
  handler = function(input, ctx)
    local id = input.task_id
    if not id then
      return { llm_output = "error: task_id is required", is_error = true }
    end
    local job = bg_jobs[id]
    if not job then
      return { llm_output = 'error: unknown task_id "' .. id .. '"', is_error = true }
    end

    local config = ctx:config()
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    local output = table.concat(job.output_parts)
    output = compress_output(output)
    output = truncate(output, max_lines, max_bytes)

    local status_line = "status: " .. job.status
    if job.exit_code then
      status_line = status_line .. " (exit code: " .. job.exit_code .. ")"
    end

    local llm_output
    if output == "" then
      llm_output = status_line .. "\nno output yet"
    else
      llm_output = status_line .. "\n" .. output
    end

    return { llm_output = llm_output, is_error = job.status == "exited" and (job.exit_code or 0) ~= 0 }
  end,
})

-- bash_watch: wait for a pattern in a background task's output
craft.api.register_tool({
  name = "bash_watch",
  kind = "execute",
  description = "Wait for a pattern (substring or Lua pattern) in a background bash task's output, or for the task to exit. Polls until match found, task exits, or timeout.",
  schema = {
    type = "object",
    properties = {
      task_id = { type = "string", description = "The task_id returned by bash", required = true },
      pattern = { type = "string", description = "Substring or Lua pattern to wait for in task output" },
      timeout = { type = "integer", description = "Max seconds to wait (default 60)" },
    },
  },
  permission_scopes = function()
    return { scopes = { "bash_watch" }, force_prompt = false }
  end,
  header = function(input)
    return "watch " .. (input.task_id or "?")
  end,
  handler = function(input, ctx)
    local id = input.task_id
    if not id then
      return { llm_output = "error: task_id is required", is_error = true }
    end
    local job = bg_jobs[id]
    if not job then
      return { llm_output = 'error: unknown task_id "' .. id .. '"', is_error = true }
    end

    local config = ctx:config()
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)
    local timeout_secs = input.timeout or 60
    local pattern = input.pattern

    ctx:set_deadline(timeout_secs)

    -- Check current output immediately
    local current = table.concat(job.output_parts)

    if pattern and current:find(pattern) then
      current = compress_output(current)
      current = truncate(current, max_lines, max_bytes)
      return {
        llm_output = "pattern found\n" .. current,
        is_error = false,
      }
    end

    if job.status == "exited" then
      current = compress_output(current)
      current = truncate(current, max_lines, max_bytes)
      local llm_output = "task exited (code: " .. (job.exit_code or "?") .. ")"
      if current ~= "" then
        llm_output = llm_output .. "\n" .. current
      end
      return { llm_output = llm_output, is_error = (job.exit_code or 0) ~= 0 }
    end

    -- No match yet and task still running. Use craft.fn.jobwait to block
    -- until the job exits or timeout, then check output.
    local result = craft.fn.jobwait(job.job_id, timeout_secs * 1000)

    -- jobwait read events directly, bypassing on_stdout/on_stderr callbacks.
    -- Merge any output from jobwait into output_parts so bash_status sees it.
    if result then
      if result.stdout ~= "" then
        for line in result.stdout:gmatch("[^\n]+") do
          append_line(job.output_parts, line)
        end
      end
      if result.stderr ~= "" then
        for line in result.stderr:gmatch("[^\n]+") do
          append_line(job.output_parts, line)
        end
      end
    end

    -- Re-read output after waiting
    local output = table.concat(job.output_parts)
    output = compress_output(output)
    output = truncate(output, max_lines, max_bytes)

    if result then
      -- jobwait returned: task exited
      job.status = "exited"
      job.exit_code = result.exit_code
      local llm_output = "task exited (code: " .. result.exit_code .. ")"
      if output ~= "" then
        llm_output = llm_output .. "\n" .. output
      end
      if pattern and output:find(pattern) then
        llm_output = "pattern found (task exited)\n" .. output
      end
      return { llm_output = llm_output, is_error = result.exit_code ~= 0 }
    else
      -- jobwait timed out: task still running
      if pattern and output:find(pattern) then
        return {
          llm_output = "pattern found (task still running)\n" .. output,
          is_error = false,
        }
      end
      return {
        llm_output = "timed out after " .. timeout_secs .. "s (task still running)\n" .. output,
        is_error = false,
      }
    end
  end,
})

-- bash_kill: terminate a background task
craft.api.register_tool({
  name = "bash_kill",
  kind = "execute",
  description = "Terminate a background bash task.",
  schema = {
    type = "object",
    properties = {
      task_id = { type = "string", description = "The task_id returned by bash", required = true },
    },
  },
  permission_scopes = function()
    return { scopes = { "bash_kill" }, force_prompt = true }
  end,
  header = function(input)
    return "kill " .. (input.task_id or "?")
  end,
  handler = function(input, ctx)
    local id = input.task_id
    if not id then
      return { llm_output = "error: task_id is required", is_error = true }
    end
    local job = bg_jobs[id]
    if not job then
      return { llm_output = 'error: unknown task_id "' .. id .. '"', is_error = true }
    end

    if job.status == "exited" then
      return { llm_output = "task already exited (code: " .. (job.exit_code or "?") .. ")", is_error = false }
    end

    craft.fn.jobstop(job.job_id)
    job.status = "killed"

    return { llm_output = "task " .. id .. " killed", is_error = false }
  end,
})

craft.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Reserve bash for system commands (git, builds, tests). Do NOT use bash for file operations, including on files outside the working dir.",
})
