value = await bash(command='printf alpha')
value['stderr'] == ''
