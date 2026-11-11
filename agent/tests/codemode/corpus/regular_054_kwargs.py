value = await glob(pattern='*.rs')
[path for path in value if path.endswith('.rs')]
