import json
value = await read_text(path='data.json')
sum(json.loads(value)["items"])
