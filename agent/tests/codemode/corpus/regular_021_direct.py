import json
value = await read_text('data.json')
json.loads(value)["name"]
