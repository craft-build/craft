async def run():
    return await list_dir('.')
value = await run()
len(value)
