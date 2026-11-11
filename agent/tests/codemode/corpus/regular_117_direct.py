paths = await glob('*')
values = []
for path in paths:
    values.append(await read_text(path))
sum(len(text) for text in values)
