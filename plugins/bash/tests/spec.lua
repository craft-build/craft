-- Exercises craft.image_uri, the helper the bash plugin uses to turn a
-- data:image/...;base64,... URI in stdout into an image content block.

local extract = require("craft.image_uri")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    failures[#failures + 1] = name .. ": " .. tostring(err)
  end
end

local TINY_PNG_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

case("extracts_image_from_bare_data_uri", function()
  local raw = "data:image/png;base64," .. TINY_PNG_B64
  local result = extract(raw)
  assert(result, "expected an image result")
  assert(result.media_type == "image/png", "media_type: " .. tostring(result.media_type))
  assert(result.data == TINY_PNG_B64, "data should round-trip for a small image")
  assert(result.caption:find("1x1"), "generated caption describes dimensions: " .. tostring(result.caption))
end)

case("preserves_surrounding_text_as_caption", function()
  local raw = "rendering chart...\ndata:image/png;base64," .. TINY_PNG_B64 .. "\ndone"
  local result = extract(raw)
  assert(result, "expected an image result")
  assert(result.caption:find("rendering chart"), "caption keeps preceding text: " .. tostring(result.caption))
  assert(result.caption:find("done"), "caption keeps trailing text: " .. tostring(result.caption))
end)

case("returns_nil_when_no_uri", function()
  assert(extract("just some plain text") == nil, "no URI -> nil")
  assert(extract("") == nil, "empty -> nil")
end)

case("returns_nil_for_unsupported_media_type", function()
  local raw = "data:image/bmp;base64," .. TINY_PNG_B64
  assert(extract(raw) == nil, "image/bmp is not supported -> nil")
end)

case("returns_nil_for_corrupt_payload", function()
  local raw = "data:image/png;base64,@@@not valid base64@@@"
  assert(extract(raw) == nil, "corrupt base64 -> nil")
end)

case("returns_nil_for_non_image_payload", function()
  local raw = "data:image/png;base64," .. craft.base64.encode("definitely not a png")
  assert(extract(raw) == nil, "valid base64 of a non-image -> nil")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
