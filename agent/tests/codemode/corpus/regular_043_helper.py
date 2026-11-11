async def run():
    return await read_text('src/alpha.rs')
value = await run()
print(value.count('alpha'))
len(value)
