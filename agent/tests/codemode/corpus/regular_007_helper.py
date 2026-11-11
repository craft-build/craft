async def run():
    return await read_text('src/alpha.rs')
value = await run()
value.count("needle")
