import asyncio
try:
    paths = await glob('*')
    values = await asyncio.gather(*[read_text(path) for path in paths])
except ToolError:
    values = []
sum(len(text) for text in values)
