import asyncio
async def run():
    return await asyncio.gather(read_text('src/alpha.rs'), read_text('src/beta.rs'))
value = await run()
[len(text) for text in value]
