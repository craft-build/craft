Mirror Neovim's Lua API namespaces (craft.uv = vim.uv, craft.fs = vim.fs, craft.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and craft.
Only exception is the UI API, neovim's has baggage.
