value = await bash('printf alpha')
len(value['stdout'].splitlines())
