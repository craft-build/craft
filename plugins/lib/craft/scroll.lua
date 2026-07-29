local function scroll(delta)
  local view, err = craft.fn.winsaveview()
  if not view then
    return nil, err
  end
  return craft.fn.winrestview({ topline = view.topline + delta })
end

return scroll
