local M = {}

M.MAX_LINES_PER_FILE = 200
M.MAX_DIR_BYTES = 50 * 1024
M.VECTORS_FILE = ".vectors.json"
M.SEMANTIC_SEARCH_TOP_K = 5
M.SEMANTIC_SIMILARITY_THRESHOLD = 0.3
M.KEYWORD_SEARCH_TOP_K = 5
M.KEYWORD_MIN_SCORE = 0.0
M.MAX_HINT_FILES = 20
M.MAX_HINT_BYTES = 1200

local STOPWORDS = {}
for _, w in ipairs({
  "the", "a", "an", "and", "or", "but", "if", "then", "else", "for", "of", "to",
  "in", "on", "at", "by", "with", "from", "is", "are", "was", "were", "be",
  "been", "being", "this", "that", "these", "those", "it", "as", "not", "no",
  "do", "does", "did", "has", "have", "had", "will", "would", "can", "could",
  "should", "may", "might", "must", "i", "you", "he", "she", "we", "they",
  "them", "his", "her", "its", "our", "their", "your", "my", "me", "us", "so",
  "than", "too", "very", "into", "about", "over", "under", "out", "up", "down",
  "all", "any", "some", "each", "both", "more", "most", "other", "such", "only",
  "own", "same", "use", "used", "using", "via", "per", "etc", "there", "here",
  "when", "where", "which", "who", "whom", "what", "why", "how",
}) do
  STOPWORDS[w] = true
end

local function tokenize(s)
  local tokens = {}
  for word in (s .. " "):lower():gmatch("([a-z0-9_]+)") do
    if #word > 1 and not STOPWORDS[word] then
      tokens[#tokens + 1] = word
    end
  end
  return tokens
end

function M.tokenize(s)
  return tokenize(s)
end

-- Lua's bit32 is 32-bit only, so we split the 64-bit FNV-1a state into
-- hi/lo halves and propagate carries by hand during multiplication.
function M.fnv1a_64(data)
  local lo = 0x84222325
  local hi = 0xcbf29ce4
  local p_lo = 0x000001b3
  local p_hi = 0x00000100
  for i = 1, #data do
    lo = bit32.bxor(lo, string.byte(data, i))
    local ll = lo * p_lo
    local ll_lo = ll % 0x100000000
    local ll_hi = (ll - ll_lo) / 0x100000000
    local new_hi = (hi * p_lo + lo * p_hi + ll_hi) % 0x100000000
    lo = ll_lo
    hi = new_hi
  end
  return string.format("%08x%08x", hi, lo)
end

function M.count_lines(s)
  if s == "" then
    return 1
  end
  local _, newlines = s:gsub("\n", "")
  if s:sub(-1) == "\n" then
    return math.max(newlines, 1)
  end
  return newlines + 1
end

function M.project_id(path)
  local base = craft.fs.basename(path) or "root"
  return base .. "-" .. M.fnv1a_64(path)
end

function M.safe_resolve(memories_dir, relative)
  if not relative or relative == "" then
    return nil, "path is required"
  end
  if relative:find("\0") or relative:sub(1, 1) == "/" then
    return nil, "path must be relative"
  end
  local resolved = craft.fs.normalize(craft.fs.joinpath(memories_dir, relative))
  local norm_base = craft.fs.normalize(memories_dir)
  local prefix = norm_base .. "/"
  if resolved:sub(1, #prefix) ~= prefix then
    return nil, "path traversal outside memories directory is not allowed"
  end
  return resolved
end

function M.has_embed()
  return craft.embed ~= nil
end

function M.load_vectors(dir)
  if not M.has_embed() then
    return {}
  end
  local path = craft.fs.joinpath(dir, M.VECTORS_FILE)
  local ok, raw = pcall(craft.fs.read, path)
  if not ok then
    return {}
  end
  local ok2, data = pcall(craft.json.decode, raw)
  if not ok2 or type(data) ~= "table" then
    return {}
  end
  return data
end

function M.save_vectors(dir, vectors)
  if not M.has_embed() then
    return
  end
  local path = craft.fs.joinpath(dir, M.VECTORS_FILE)
  local encoded = craft.json.encode(vectors)
  craft.fs.write(path, encoded)
end

function M.store_embedding(dir, filename, content)
  if not M.has_embed() then
    return
  end
  local ok, vec = pcall(craft.embed.embed, content)
  if not ok or not vec then
    return
  end
  local vectors = M.load_vectors(dir)
  vectors[filename] = vec
  M.save_vectors(dir, vectors)
end

function M.remove_embedding(dir, filename)
  if not M.has_embed() then
    return
  end
  local vectors = M.load_vectors(dir)
  vectors[filename] = nil
  M.save_vectors(dir, vectors)
end

local function cosine_similarity(a, b)
  if #a ~= #b or #a == 0 then
    return 0.0
  end
  return craft.embed.similarity(a, b)
end

function M.semantic_search(dir, query, top_k)
  if not M.has_embed() then
    return nil, "semantic search requires onnx feature"
  end
  local ok, query_vec = pcall(craft.embed.embed, query)
  if not ok or not query_vec then
    return nil, "failed to embed query"
  end
  local vectors = M.load_vectors(dir)
  local scored = {}
  for filename, vec in pairs(vectors) do
    local sim = cosine_similarity(query_vec, vec)
    if sim >= M.SEMANTIC_SIMILARITY_THRESHOLD then
      scored[#scored + 1] = { filename, sim }
    end
  end
  table.sort(scored, function(a, b)
    return a[2] > b[2]
  end)
  local k = top_k or M.SEMANTIC_SEARCH_TOP_K
  local results = {}
  for i = 1, math.min(#scored, k) do
    results[#results + 1] = scored[i]
  end
  return results
end

function M.keyword_search(dir, query, top_k)
  local entries = M.collect_file_entries(dir)
  if #entries == 0 then
    return {}
  end
  local query_terms = tokenize(query)
  if #query_terms == 0 then
    return {}
  end
  local docs = {}
  local df = {}
  local n = 0
  for _, entry in ipairs(entries) do
    local filename = entry[1]
    local fp = M.safe_resolve(dir, filename)
    if fp then
      local ok, content = pcall(craft.fs.read, fp)
      if ok and content then
        local terms = tokenize(content)
        local tf = {}
        for _, t in ipairs(terms) do
          tf[t] = (tf[t] or 0) + 1
        end
        docs[filename] = { tf = tf, len = #terms }
        n = n + 1
        for t in pairs(tf) do
          df[t] = (df[t] or 0) + 1
        end
      end
    end
  end
  local scored = {}
  for filename, doc in pairs(docs) do
    local score = 0.0
    for _, term in ipairs(query_terms) do
      local f = doc.tf[term]
      if f then
        local idf = math.log(1 + n / df[term])
        score = score + f * idf
      end
    end
    if score > M.KEYWORD_MIN_SCORE then
      local norm = doc.len > 0 and doc.len or 1
      scored[#scored + 1] = { filename, score / norm }
    end
  end
  table.sort(scored, function(a, b)
    return a[2] > b[2]
  end)
  local k = top_k or M.KEYWORD_SEARCH_TOP_K
  local results = {}
  for i = 1, math.min(#scored, k) do
    results[#results + 1] = scored[i]
  end
  return results
end

function M.cleanup_vectors(dir)
  if not M.has_embed() then
    return
  end
  local vectors = M.load_vectors(dir)
  local changed = false
  for filename, _ in pairs(vectors) do
    local fp = M.safe_resolve(dir, filename)
    if not fp or not craft.fs.metadata(fp) then
      vectors[filename] = nil
      changed = true
    end
  end
  if changed then
    M.save_vectors(dir, vectors)
  end
end

function M.collect_file_entries(dir)
  local entries = craft.fs.dir(dir)
  if not entries then
    return {}
  end
  local files = {}
  for _, entry in ipairs(entries) do
    if entry[2] == "file" and entry[1] ~= M.VECTORS_FILE then
      local meta = craft.fs.metadata(craft.fs.joinpath(dir, entry[1]))
      if meta then
        files[#files + 1] = { entry[1], meta.size }
      end
    end
  end
  return files
end

function M.dir_total_bytes(dir)
  local total = 0
  for _, f in ipairs(M.collect_file_entries(dir)) do
    total = total + f[2]
  end
  return total
end

function M.list_memories(dir)
  local files = M.collect_file_entries(dir)
  if #files == 0 then
    return "No memories yet."
  end
  table.sort(files, function(a, b)
    return a[1] < b[1]
  end)
  local lines = {}
  local total = 0
  for _, f in ipairs(files) do
    lines[#lines + 1] = f[1] .. " (" .. f[2] .. " bytes)"
    total = total + f[2]
  end
  lines[#lines + 1] = ""
  lines[#lines + 1] = #files .. " files, " .. total .. " bytes total"
  return table.concat(lines, "\n")
end

return M
