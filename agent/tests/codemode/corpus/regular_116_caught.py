import asyncio
try:
    value = await asyncio.gather(read_text('src/alpha.rs'), read_text('src/beta.rs'))
except ToolError as error:
    value = str(error)
[len(text) for text in value]
