try:
    value = await edit('src/alpha.rs', 'needle', '')
except ToolError as error:
    value = str(error)
value
