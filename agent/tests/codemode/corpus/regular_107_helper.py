async def run():
    return await edit('docs/guide.md', 'Guide', 'Handbook')
value = await run()
value
