value = await glob('*.md')
[path for path in value if path.endswith('.md')]
