-- Extract an image from a data URI embedded in bash stdout. Returns
-- { media_type, data, caption } (data is base64), or nil on any failure.
-- Constants mirror plugins/view_image so both paths agree on provider limits.

local MAX_RAW_BYTES = 3 * 1024 * 1024
local MAX_EDGE = 1568

local MEDIA_TYPES = {
  png = "image/png",
  jpeg = "image/jpeg",
  gif = "image/gif",
  webp = "image/webp",
}

local DATA_URI = "data:image/(%a+);base64,([^%s]*)"

local function format_size(bytes)
  if bytes >= 1024 * 1024 then
    return string.format("%.1fMB", bytes / (1024 * 1024))
  end
  return string.format("%dKB", math.ceil(bytes / 1024))
end

local function caption_for(raw, uri_start, uri_end, width, height, bytes, note)
  local before = raw:sub(1, uri_start - 1)
  local after = raw:sub(uri_end + 1)
  local surrounding = (before .. after):match("^%s*(.-)%s*$")
  if surrounding ~= "" then
    return surrounding
  end
  return string.format("[image: %dx%d, %s%s]", width, height, format_size(bytes), note or "")
end

local function fit_to_limits(bytes, info)
  if #bytes <= MAX_RAW_BYTES and math.max(info.width, info.height) <= MAX_EDGE then
    return bytes, info.media_type, ""
  end

  local img = craft.image.decode(bytes)
  if not img then
    return nil, nil, nil
  end

  local resized = math.max(info.width, info.height) > MAX_EDGE
  if resized then
    img = img:resize(MAX_EDGE, MAX_EDGE)
  end

  local out_format = info.format == "jpeg" and "jpeg" or "png"
  local encoded = img:encode(out_format)
  if #encoded > MAX_RAW_BYTES and out_format == "png" then
    out_format = "jpeg"
    encoded = img:encode(out_format)
  end
  if #encoded > MAX_RAW_BYTES then
    return nil, nil, nil
  end

  local note = resized and string.format(", downscaled from %dx%d", info.width, info.height) or ", re-encoded"
  if info.format == "gif" or info.format == "webp" then
    note = note .. ", first frame only"
  end
  return encoded, MEDIA_TYPES[out_format], note
end

-- Returns { media_type, data, caption } or nil. Any host-side error during
-- decode/resize/encode collapses to nil so the caller falls back to text.
local function extract(raw)
  local uri_start, uri_end, ext, payload = raw:find(DATA_URI)
  if not uri_start or not payload or payload == "" then
    return nil
  end

  local format = ext:lower()
  local media_type = MEDIA_TYPES[format]
  if not media_type then
    return nil
  end

  local ok, result = pcall(function()
    local bytes = craft.base64.decode(payload)
    if not bytes then
      return nil
    end

    local info = craft.image.probe(bytes)
    if not info then
      return nil
    end
    info.media_type = media_type
    info.format = format

    local fitted, fitted_media, note = fit_to_limits(bytes, info)
    if not fitted then
      return nil
    end

    return {
      media_type = fitted_media,
      data = craft.base64.encode(fitted),
      caption = caption_for(raw, uri_start, uri_end, info.width, info.height, #fitted, note),
    }
  end)
  if not ok then
    return nil
  end
  return result
end

return extract
