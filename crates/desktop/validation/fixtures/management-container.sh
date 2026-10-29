#!/bin/sh
# Isolated protocol fixture; it never contacts a container daemon.
set -eu
root=ROOT
printf '<%s>\n' "$@" >> "$root/container-arguments"
[ "$1" = container ] || exit 99
verb=$2; shift 2
case "$verb" in
 inspect)
  if [ ! -f "$root/exists" ]; then printf 'Error: No such container' >&2; exit 1; fi
  running=false; [ ! -f "$root/running" ] || running=true
  labels='{}'; [ ! -f "$root/labels" ] || labels=$(cat "$root/labels")
  printf '{"Id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Config":{"Labels":%s},"State":{"Running":%s}}' "$labels" "$running"
  ;;
 create)
  owner=''; profile=''; operation=''; previous=''
  for arg in "$@"; do
    if [ "$previous" = '--label' ]; then
      case "$arg" in com.qurkos.xylo.installation=*) owner=${arg#*=};; com.qurkos.xylo.environment=*) profile=${arg#*=};; com.qurkos.xylo.create-operation=*) operation=${arg#*=};; esac
    fi
    previous=$arg
  done
  printf '{"com.qurkos.xylo.installation":"%s","com.qurkos.xylo.environment":"%s","com.qurkos.xylo.create-operation":"%s"}' "$owner" "$profile" "$operation" > "$root/labels"
  touch "$root/exists"
  printf aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  ;;
 start) touch "$root/running";;
 stop) rm -f "$root/running";;
 rm) rm -f "$root/exists" "$root/labels";;
 exec)
  printf 'executed\n' >> "$root/executions"
  case "$*" in *'__hang__'*) touch "$root/dispatched"; sleep 20;; *) printf 'isolated native command completed';; esac
  ;;
 *) exit 98;;
esac
