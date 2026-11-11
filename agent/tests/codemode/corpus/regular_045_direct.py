import json
value = await read_text('data.json')
len(json.loads(value)["items"])
