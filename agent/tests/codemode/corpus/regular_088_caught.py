try:
    value = await bash('printf alpha')
except ToolError as error:
    value = str(error)
len(value['stdout'].splitlines())
