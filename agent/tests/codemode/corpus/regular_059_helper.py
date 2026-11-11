async def run():
    return await glob('*.md')
value = await run()
[path for path in value if path.endswith('.md')]
