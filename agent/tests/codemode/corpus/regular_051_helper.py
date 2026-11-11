async def run():
    return await glob('*')
value = await run()
len(value)
