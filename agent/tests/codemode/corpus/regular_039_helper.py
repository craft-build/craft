async def run():
    return await read('src/alpha.rs', 2, 1)
value = await run()
value
