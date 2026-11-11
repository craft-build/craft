async def run():
    return await edit('src/alpha.rs', 'alpha', 'gamma')
value = await run()
value
