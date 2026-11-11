import re
value = await read_text('src/alpha.rs')
len(re.findall("alpha", value))
