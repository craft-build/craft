try:
    value = await edit('src/beta.rs', 'beta', 'delta')
except ToolError as error:
    value = str(error)
value
