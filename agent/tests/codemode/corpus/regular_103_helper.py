async def run():
    return await edit('src/beta.rs', 'beta', 'delta')
value = await run()
value
