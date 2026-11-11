value = await glob(pattern='*.md')
[path for path in value if path.endswith('.md')]
