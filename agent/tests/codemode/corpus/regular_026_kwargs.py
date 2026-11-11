import re
value = await read_text(path='src/alpha.rs')
len(re.findall("alpha", value))
