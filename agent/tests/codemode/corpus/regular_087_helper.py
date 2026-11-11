async def run():
    return await bash('printf alpha')
value = await run()
len(value['stdout'].splitlines())
