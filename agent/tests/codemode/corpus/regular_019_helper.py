import json
async def run():
    return await read_text('data.json')
value = await run()
sum(json.loads(value)["items"])
