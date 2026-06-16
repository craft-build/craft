return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local truncated_msg = U.truncated_msg
  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD

  local function strip_quotes(s)
    return s:match("^[\"'](.+)[\"']$") or s
  end

  local function split_dart_path(s)
    s = s:gsub("%.dart$", "")
    s = s:gsub("^dart:", "dart/")
    s = s:gsub("^package:", "")
    local path = {}
    for part in s:gmatch("[^/]+") do
      path[#path + 1] = part
    end
    return path
  end

  local function resolve_uri(node)
    local uri_child = find_child(node, "uri")
    if uri_child then
      return find_child(uri_child, "string_literal") or uri_child
    end
    return node
  end

  local function extract_import(node, source)
    local uri_field = node:field("uri")[1]
    if not uri_field then
      return nil
    end
    local uri_str = resolve_uri(uri_field)
    local uri = strip_quotes(get_text(uri_str, source))
    local paths = { split_dart_path(uri) }

    local alias_node = node:field("alias")[1]
    if alias_node then
      paths[1][#paths[1] + 1] = "as " .. get_text(alias_node, source)
    end

    for _, child in ipairs(node:children()) do
      if child:type() == "combinator" then
        paths[1][#paths[1] + 1] = get_text(child, source)
      end
    end

    return new_import_entry(node, paths)
  end

  local function extract_export(node, source)
    local uri_field = node:field("uri")[1]
    if not uri_field then
      return nil
    end
    local uri_str = resolve_uri(uri_field)
    local uri = strip_quotes(get_text(uri_str, source))
    local paths = { split_dart_path(uri) }

    for _, child in ipairs(node:children()) do
      if child:type() == "combinator" then
        paths[1][#paths[1] + 1] = get_text(child, source)
      end
    end

    return new_import_entry(node, paths, "export")
  end

  local function extract_part(node, source)
    local uri_field = node:field("uri")[1]
    if not uri_field then
      return nil
    end
    local uri = strip_quotes(get_text(resolve_uri(uri_field), source))
    local paths = { split_dart_path(uri) }
    return new_import_entry(node, paths, "part")
  end

  local function extract_part_of(node, source)
    local dotted = find_child(node, "dotted_identifier_list")
    if dotted then
      return new_entry(SECTION.Module, node, get_text(dotted, source))
    end
    local uri_field = node:field("uri")[1]
    if uri_field then
      return new_entry(SECTION.Module, node, strip_quotes(get_text(resolve_uri(uri_field), source)))
    end
    local text = get_text(node, source)
    local cleaned = text:match("^part%s+of%s+(.-)%s*;?%s*$") or text
    return new_entry(SECTION.Module, node, cleaned)
  end

  local function extract_library_name(node, source)
    local dotted = find_child(node, "dotted_identifier_list")
    if dotted then
      return new_entry(SECTION.Module, node, get_text(dotted, source))
    end
    local text = get_text(node, source)
    local cleaned = text:match("^library%s+(.-)%s*;?%s*$") or text
    return new_entry(SECTION.Module, node, cleaned)
  end

  local function fn_sig(node, source)
    local ret_node = node:field("return_type")[1]
    local ret = ret_node and (get_text(ret_node, source) .. " ") or ""
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local params_nodes = node:field("parameters")
    local params = "()"
    for _, pn in ipairs(params_nodes) do
      if pn:type() == "formal_parameter_list" then
        params = get_text(pn, source)
        break
      end
    end
    return compact_ws(ret .. name .. params)
  end

  local function getter_sig(node, source)
    local ret_node = node:field("return_type")[1]
    local ret = ret_node and (get_text(ret_node, source) .. " ") or ""
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    return compact_ws(ret .. "get " .. name)
  end

  local function setter_sig(node, source)
    local ret_node = node:field("return_type")[1]
    local ret = ret_node and (get_text(ret_node, source) .. " ") or ""
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local params_nodes = node:field("parameters")
    local params = "()"
    for _, pn in ipairs(params_nodes) do
      if pn:type() == "formal_parameter_list" then
        params = get_text(pn, source)
        break
      end
    end
    return compact_ws(ret .. "set " .. name .. params)
  end

  local function constructor_sig(node, source)
    local params_node = node:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    local parts = {}
    for _, child in ipairs(node:field("name")) do
      if child:type() == "identifier" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    local name = table.concat(parts, ".")
    return compact_ws(name .. params)
  end

  local function extract_method_sig(sig_node, source)
    local sk = sig_node:type()
    if sk == "function_signature" then
      return fn_sig(sig_node, source)
    elseif sk == "getter_signature" then
      return getter_sig(sig_node, source)
    elseif sk == "setter_signature" then
      return setter_sig(sig_node, source)
    elseif sk == "constructor_signature" or sk == "factory_constructor_signature" then
      return constructor_sig(sig_node, source)
    elseif sk == "operator_signature" then
      return get_text(sig_node, source)
    end
    return compact_ws(get_text(sig_node, source))
  end

  local function class_members(body, source)
    local members = {}
    local field_count = 0
    for _, child in ipairs(body:children()) do
      local ck = child:type()
      if ck == "class_member" then
        for _, member in ipairs(child:children()) do
          local mk = member:type()
          if mk == "method_declaration" then
            local sig_node = member:field("signature")[1]
            if sig_node then
              local inner_sig = find_child(sig_node, "function_signature")
                or find_child(sig_node, "getter_signature")
                or find_child(sig_node, "setter_signature")
                or find_child(sig_node, "operator_signature")
                or find_child(sig_node, "constructor_signature")
                or find_child(sig_node, "factory_constructor_signature")
                or find_child(sig_node, "redirecting_factory_constructor_signature")
              if inner_sig then
                local sig = extract_method_sig(inner_sig, source)
                if sig then
                  local lr = format_range(line_start(member), line_end(member))
                  members[#members + 1] = compact_ws(sig) .. " " .. lr
                end
              end
            end
          elseif mk == "declaration" then
            field_count = field_count + 1
            if field_count <= FIELD_TRUNCATE_THRESHOLD then
              local text = compact_ws(get_text(member, source))
              if #text > 80 then
                text = text:sub(1, 69) .. "[truncated]"
              end
              local lr = format_range(line_start(member), line_end(member))
              members[#members + 1] = text .. " " .. lr
            end
          end
        end
      end
    end
    if field_count > FIELD_TRUNCATE_THRESHOLD then
      members[#members + 1] = truncated_msg(field_count)
    end
    return members
  end

  local function tparams_text(node, source)
    local tp = node:field("type_parameters")[1]
    if not tp then
      tp = find_child(node, "type_parameters")
    end
    return tp and get_text(tp, source) or ""
  end

  local function extract_superclass(node, source)
    local super_node = node:field("superclass")[1]
    if not super_node then
      return ""
    end
    local type_nodes = super_node:field("type")
    local type_str = ""
    if type_nodes and #type_nodes > 0 then
      type_str = " extends " .. get_text(type_nodes[1], source)
    end
    local mixins_node = find_child(super_node, "mixins")
    local mixins_str = mixins_node and (" with " .. get_text(mixins_node, source)) or ""
    return type_str .. mixins_str
  end

  local function extract_interfaces(node, source)
    local ifaces = node:field("interfaces")[1]
    if not ifaces then
      return ""
    end
    return " implements " .. get_text(ifaces, source)
  end

  local function extract_class(node, source)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local super = extract_superclass(node, source)
    local ifaces = extract_interfaces(node, source)
    local label = compact_ws("class " .. name .. tparams .. super .. ifaces)
    local entry = new_entry(SECTION.Class, node, label)
    local body = node:field("body")[1]
    if body then
      entry.children = class_members(body, source)
    end
    return entry
  end

  local function extract_enum(node, source)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local ifaces = extract_interfaces(node, source)
    local label = compact_ws("enum " .. name .. tparams .. ifaces)
    local entry = new_entry(SECTION.Type, node, label)
    local body = node:field("body")[1]
    if body then
      local variants = {}
      local variant_count = 0
      for _, child in ipairs(body:children()) do
        if child:type() == "enum_constant" then
          variant_count = variant_count + 1
          local name_n = child:field("name")[1]
          variants[#variants + 1] = name_n and get_text(name_n, source) or "_"
        end
      end
      entry.children = variants
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_mixin(node, source)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local on_str = ""
    for _, child in ipairs(node:children()) do
      if child:type() == "type" then
        on_str = " on " .. get_text(child, source)
        break
      end
    end
    local ifaces = extract_interfaces(node, source)
    local label = compact_ws("mixin " .. name .. tparams .. on_str .. ifaces)
    local entry = new_entry(SECTION.Trait, node, label)
    local body = node:field("body")[1]
    if body then
      entry.children = class_members(body, source)
    end
    return entry
  end

  local function extract_extension(node, source)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or ""
    local tparams = tparams_text(node, source)
    local on_type = node:field("class")[1]
    local on_str = on_type and (" on " .. get_text(on_type, source)) or ""
    local label
    if name ~= "" then
      label = compact_ws("extension " .. name .. tparams .. on_str)
    else
      label = compact_ws("extension" .. tparams .. on_str)
    end
    local entry = new_entry(SECTION.Impl, node, label)
    local body = node:field("body")[1]
    if body then
      entry.children = class_members(body, source)
    end
    return entry
  end

  local function extract_extension_type(node, source)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local rep = node:field("representation")[1]
    local rep_str = rep and (" " .. get_text(rep, source)) or ""
    local ifaces = extract_interfaces(node, source)
    local label = compact_ws("extension type " .. name .. tparams .. rep_str .. ifaces)
    local entry = new_entry(SECTION.Class, node, label)
    local body = node:field("body")[1]
    if body then
      entry.children = class_members(body, source)
    end
    return entry
  end

  local function extract_type_alias(node, source)
    local name_node
    for _, child in ipairs(node:children()) do
      if child:type() == "type_identifier" then
        name_node = child
        break
      end
    end
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local type_str = ""
    for _, child in ipairs(node:children()) do
      if child:type() == "type" then
        type_str = " = " .. get_text(child, source)
        break
      end
    end
    return new_entry(SECTION.Type, node, compact_ws("typedef " .. name .. tparams .. type_str))
  end

  local function extract_function_decl(node, source)
    local sig_node = node:field("signature")[1]
    if not sig_node then
      return nil
    end
    local sig = fn_sig(sig_node, source)
    return sig and new_entry(SECTION.Function, node, sig) or nil
  end

  local function extract_getter_decl(node, source)
    local sig_node = node:field("signature")[1]
    if not sig_node then
      return nil
    end
    local sig = getter_sig(sig_node, source)
    return sig and new_entry(SECTION.Function, node, sig) or nil
  end

  local function extract_setter_decl(node, source)
    local sig_node = node:field("signature")[1]
    if not sig_node then
      return nil
    end
    local sig = setter_sig(sig_node, source)
    return sig and new_entry(SECTION.Function, node, sig) or nil
  end

  local function extract_top_level_var(node, source)
    local entries = {}
    for _, child in ipairs(node:children()) do
      local ck = child:type()
      if ck == "initialized_identifier_list" then
        for _, id in ipairs(child:children()) do
          if id:type() == "initialized_identifier" then
            local name_node = id:field("name")[1]
            if name_node then
              local name = get_text(name_node, source)
              if name:match("^[A-Z_][A-Z0-9_]*$") then
                entries[#entries + 1] = new_entry(SECTION.Constant, id, "const " .. name)
              end
            end
          end
        end
      elseif ck == "static_final_declaration_list" then
        for _, decl in ipairs(child:children()) do
          local decl_text = get_text(decl, source)
          local name = decl_text:match("^%s*(%S+)%s")
          if name then
            entries[#entries + 1] = new_entry(SECTION.Constant, decl, "const " .. name)
          end
        end
      end
    end
    return entries
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, _source)
      return node:type() == "documentation_block_comment"
    end,

    is_attr = function(node)
      return node:type() == "annotation"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "import_or_export" then
        local results = {}
        for _, child in ipairs(node:children()) do
          local ck = child:type()
          if ck == "library_import" then
            for _, imp in ipairs(child:children()) do
              if imp:type() == "import_specification" then
                local e = extract_import(imp, source)
                if e then
                  results[#results + 1] = e
                end
              end
            end
          elseif ck == "library_export" then
            local e = extract_export(child, source)
            if e then
              results[#results + 1] = e
            end
          end
        end
        return results
      elseif kind == "part_directive" then
        local e = extract_part(node, source)
        return e and { e } or {}
      elseif kind == "part_of_directive" then
        return { extract_part_of(node, source) }
      elseif kind == "library_name" then
        return { extract_library_name(node, source) }
      elseif kind == "function_declaration" then
        local e = extract_function_decl(node, source)
        return e and { e } or {}
      elseif kind == "getter_declaration" then
        local e = extract_getter_decl(node, source)
        return e and { e } or {}
      elseif kind == "setter_declaration" then
        local e = extract_setter_decl(node, source)
        return e and { e } or {}
      elseif kind == "top_level_variable_declaration" then
        return extract_top_level_var(node, source)
      elseif kind == "class_declaration" then
        local e = extract_class(node, source)
        return e and { e } or {}
      elseif kind == "mixin_declaration" then
        local e = extract_mixin(node, source)
        return e and { e } or {}
      elseif kind == "extension_declaration" then
        local e = extract_extension(node, source)
        return e and { e } or {}
      elseif kind == "extension_type_declaration" then
        local e = extract_extension_type(node, source)
        return e and { e } or {}
      elseif kind == "enum_declaration" then
        local e = extract_enum(node, source)
        return e and { e } or {}
      elseif kind == "type_alias" then
        local e = extract_type_alias(node, source)
        return e and { e } or {}
      end

      return {}
    end,
  }
end
