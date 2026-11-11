value = await grep(pattern='alpha')
sorted({match['path'] for match in value})
