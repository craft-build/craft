import json
value = await read_text(path='data.json')
json.loads(value)["name"]
