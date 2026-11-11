async def run():
    return await bash('printf alpha')
value = await run()
value['exit_code']
