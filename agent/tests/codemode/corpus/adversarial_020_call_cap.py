async def load():
    await read_text('src/alpha.rs')
for index in range(6):
    await load()
