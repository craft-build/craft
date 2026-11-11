try:
    value = await grep('alpha')
except ToolError as error:
    value = str(error)
sorted({match['path'] for match in value})
