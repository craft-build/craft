import json
async def run():
    return await read_text('data.json')
value = await run()
len(json.loads(value)["items"])
