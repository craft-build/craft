Mirror Neovim's Lua API namespaces (craft.uv = vim.uv, craft.fs = vim.fs, craft.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and craft.
Only exception is the UI API, neovim's has baggage.

## Design

Our goal is to let plugin authors have as much freedom as possible, that's why designing the APIs should be looked at as simple primitives you combine together.

## Error convention

Fallible runtime operations return the pair (value, err) and never throw.
Throwing is reserved for programmer errors, like passing a number where a string belongs.

Tool handlers fail with `{ llm_output = msg, is_error = true }`; a plain string is always success (only `is_error` flags the result as an error to the provider).
