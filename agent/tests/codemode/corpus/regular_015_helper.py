async def run():
    return await read_text('docs/guide.md')
value = await run()
value.upper()
