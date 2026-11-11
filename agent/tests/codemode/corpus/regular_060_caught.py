try:
    value = await glob('*.md')
except ToolError as error:
    value = str(error)
[path for path in value if path.endswith('.md')]
