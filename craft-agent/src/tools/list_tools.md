List the tools available in this session, or enable and inspect a specific tool.

Call this with no arguments to see every tool with a short description. Tools you have not enabled yet are listed under "Not yet available". To enable one and see its full input schema, call `list_tools(detail="<name>")`. Once enabled, a tool stays available for the rest of the session.

Most frequent tools (read, edit, write, grep, bash, etc.) are already available. Less common tools (review, styleguide, web, and MCP server tools) start hidden to save tokens; promote the ones you need with `detail`. If a tool call is rejected because the tool is not advertised, promote it first with this tool.
