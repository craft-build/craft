try:
    value = await edit('src/alpha.rs', 'alpha', 'gamma')
except ToolError as error:
    value = str(error)
value
