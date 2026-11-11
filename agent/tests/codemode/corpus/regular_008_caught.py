try:
    value = await read_text('src/alpha.rs')
except ToolError as error:
    value = str(error)
value.count("needle")
