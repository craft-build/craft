import json
try:
    value = await read_text('data.json')
except ToolError as error:
    value = str(error)
len(json.loads(value)["items"])
