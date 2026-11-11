try:
    value = await read_text('docs/guide.md')
except ToolError as error:
    value = str(error)
value.upper()
