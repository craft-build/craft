try:
    value = await glob('*')
except ToolError as error:
    value = str(error)
len(value)
