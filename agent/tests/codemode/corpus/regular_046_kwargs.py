import json
value = await read_text(path='data.json')
len(json.loads(value)["items"])
