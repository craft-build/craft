try:
    value = await read('src/alpha.rs', 2, 1)
except ToolError as error:
    value = str(error)
value
