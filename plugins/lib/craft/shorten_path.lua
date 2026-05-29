local function shorten_path(path)
  local cwd = craft.uv.cwd()
  if cwd and path:sub(1, #cwd + 1) == cwd .. "/" then
    local rel = path:sub(#cwd + 2)
    return rel == "" and "." or rel
  end
  local home = craft.uv.os_homedir()
  if home and path:sub(1, #home + 1) == home .. "/" then
    local rel = path:sub(#home + 2)
    return rel == "" and "~" or "~/" .. rel
  end
  return path
end

return shorten_path
