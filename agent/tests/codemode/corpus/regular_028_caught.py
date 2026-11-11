import re
try:
    value = await read_text('src/alpha.rs')
except ToolError as error:
    value = str(error)
len(re.findall("alpha", value))
