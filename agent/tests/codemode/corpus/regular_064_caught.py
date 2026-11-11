try:
    value = await grep('alpha')
except ToolError as error:
    value = str(error)
len(value)
