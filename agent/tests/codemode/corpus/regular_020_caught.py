import json
try:
    value = await read_text('data.json')
except ToolError as error:
    value = str(error)
sum(json.loads(value)["items"])
