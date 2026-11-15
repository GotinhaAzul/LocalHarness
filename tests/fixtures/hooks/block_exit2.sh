#!/bin/sh
# A blocking pre_tool hook: exit 2 is an explicit veto; stderr is the reason.
cat >/dev/null
printf 'blocked by policy: no writes on main\n' >&2
exit 2
