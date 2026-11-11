async def run():
    return await grep('alpha')
value = await run()
sum(match['line'] for match in value)
