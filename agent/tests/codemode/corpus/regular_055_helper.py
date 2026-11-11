async def run():
    return await glob('*.rs')
value = await run()
[path for path in value if path.endswith('.rs')]
