try:
    value = await read_text('src/beta.rs')
except ToolError as error:
    value = str(error)
value.strip().endswith("beta")
