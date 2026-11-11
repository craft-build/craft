import asyncio
async def load(path):
    return await read_text(path)
paths = await glob('*')
values = await asyncio.gather(*[load(path) for path in paths])
sum(len(text) for text in values)
