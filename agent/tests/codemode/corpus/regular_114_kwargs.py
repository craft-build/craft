import asyncio
value = await asyncio.gather(read_text(path='src/alpha.rs'), read_text(path='src/beta.rs'))
[len(text) for text in value]
