async def run():
    return await grep('alpha')
value = await run()
len(value)
