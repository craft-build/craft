async def run():
    return await read_text('docs/guide.md')
value = await run()
len(set(value.split()))
