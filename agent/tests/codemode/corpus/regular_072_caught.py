try:
    value = await grep('alpha')
except ToolError as error:
    value = str(error)
sum(match['line'] for match in value)
