async def run():
    return await grep('alpha')
value = await run()
sorted({match['path'] for match in value})
