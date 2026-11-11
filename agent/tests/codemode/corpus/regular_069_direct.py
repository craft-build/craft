value = await grep('alpha')
sum(match['line'] for match in value)
