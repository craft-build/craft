value = await bash('printf alpha')
value['stderr'] == ''
