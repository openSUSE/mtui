#!/bin/bash

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 [id]

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 checks the rpm dependencies of all installed packages, but with --nofiles

EOF
}

ARGS=$(getopt -o p:r:h -- "$@")

if [ $? -gt 0 ]; then help; exit 1; fi

eval set -- "$ARGS"

while true; do
   case "$1" in
      -p) shift; echo "INFO: option -p is not implemented"; shift; ;;
      -r) shift; echo "INFO: option -r is not implemented"; shift; ;;
      -h) shift; help="set" ;;
      --) shift; break; ;;
   esac
done

id="$1"

if [ -n "$help" ]; then
   help
   exit 0
fi

(while true; do sleep 60; echo "."; done) &
PONG=$!

rpm -Va --nofiles | sort >&2

kill $PONG &>/dev/null

exit 0

