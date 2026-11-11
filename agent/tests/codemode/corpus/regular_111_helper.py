async def run():
    return await edit('src/alpha.rs', 'needle', '')
value = await run()
value
