#!/bin/sh
cat >/dev/null
printf '{"decision":"allow","context":"'
head -c 20000 /dev/zero | tr '\0' 'x'
printf '"}\n'
