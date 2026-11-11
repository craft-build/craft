import asyncio
value = await asyncio.gather(read_text('src/alpha.rs'), read_text('src/beta.rs'))
[len(text) for text in value]
