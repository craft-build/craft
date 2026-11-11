async def run():
    return await read_text('src/beta.rs')
value = await run()
value.strip().endswith("beta")
