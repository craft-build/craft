Execute Python code in a sandboxed interpreter with tools as callable functions.

Use for chained/dependent tool calls and filtering/processing results, e.g. filtering web tool output. **DRAMATICALLY** cheaper than sequential tool calls!

- All tools are async and return strings: `result = await read(path='file.txt', offset=1, limit=0)`. Parse output yourself.
- Concurrency: `a, b = await gather(read(path='a.py', offset=1, limit=0), grep(pattern='x'))`. Pass calls directly, never wrapped in `async def`.
- Available libs: re, asyncio, sys, os, json. No other imports, no classes, no filesystem/network access.
- Fresh sandbox each run: no state persists between executions.
- 30s script timeout (`timeout` param); time awaiting tool calls doesn't count.
- Skip it when a single tool call needs no transformation.
- NOT a thinking scratchpad. Reason in your response text.
