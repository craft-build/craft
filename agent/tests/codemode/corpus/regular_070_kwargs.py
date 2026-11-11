value = await grep(pattern='alpha')
sum(match['line'] for match in value)
