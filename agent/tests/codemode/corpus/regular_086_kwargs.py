value = await bash(command='printf alpha')
len(value['stdout'].splitlines())
