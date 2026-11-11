try:
    value = await glob('*.rs')
except ToolError as error:
    value = str(error)
[path for path in value if path.endswith('.rs')]
