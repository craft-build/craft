try:
    value = await read_text('src/beta.rs')
except ToolError as error:
    value = str(error)
len(value.splitlines())
