#!/bin/sh
cat >/dev/null
printf 'this hook is broken\n' >&2
exit 1
