-- Herdr lifecycle reporter: reports idle/working/blocked to the herdr pane
-- craft runs in, and releases authority when the session ends. No-op outside
-- herdr.

local getenv = craft.uv.os_getenv

local SOURCE = "custom:craft"
local AGENT = "craft"
local QUESTION_TOOL = "question"
local QUESTION_BLOCKED_MSG = "waiting for user input"

local bin = getenv("HERDR_BIN_PATH")
local pane = getenv("HERDR_PANE_ID")

if getenv("HERDR_ENV") ~= "1" or not bin or not pane then
  return {}
end

craft.fn.jobstart({ bin, "-loaded" })

local seq = 0

local function herdr(args)
  seq = seq + 1
  local argv = { bin, "pane", table.unpack(args), "--seq", tostring(seq) }
  craft.fn.jobstart(argv)
end

local function report(state, ev, message)
  local args = {
    "report-agent",
    pane,
    "--source",
    SOURCE,
    "--agent",
    AGENT,
    "--state",
    state,
  }
  local sid = ev.data and ev.data.session_id
  if sid and sid ~= "" then
    args[#args + 1] = "--agent-session-id"
    args[#args + 1] = sid
  end
  if message then
    args[#args + 1] = "--message"
    args[#args + 1] = message
  end
  herdr(args)
end

local function release()
  herdr({
    "release-agent",
    pane,
    "--source",
    SOURCE,
    "--agent",
    AGENT,
  })
end

craft.api.create_autocmd("TurnStart", {
  callback = function(ev)
    report("working", ev)
  end,
})

craft.api.create_autocmd("ToolStart", {
  callback = function(ev)
    if ev.data.tool == QUESTION_TOOL then
      report("blocked", ev, QUESTION_BLOCKED_MSG)
    else
      report("working", ev)
    end
  end,
})

craft.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    report("idle", ev)
  end,
})

craft.api.create_autocmd("TurnError", {
  callback = function(ev)
    report("blocked", ev, ev.data.message)
  end,
})

craft.api.create_autocmd("SessionEnd", {
  callback = release,
})

return {}
