import asyncio
paths = await glob('*')
values = await asyncio.gather(*[read_text(path) for path in paths])
sum(len(text) for text in values)
