try:
    value = await list_dir('.')
except ToolError as error:
    value = str(error)
'Cargo.toml' in value
