import json
value = await read_text('data.json')
sum(json.loads(value)["items"])
