try:
    value = await edit('docs/guide.md', 'Guide', 'Handbook')
except ToolError as error:
    value = str(error)
value
