value = await glob('*.rs')
[path for path in value if path.endswith('.rs')]
