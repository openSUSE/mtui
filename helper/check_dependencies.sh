#!/bin/bash

export LANG=C

help () {
   cat <<EOF

usage: $0 <id>

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 checks the rpm dependencies of all installed packages, but with --nofiles

EOF
}

while getopts ":r:p:h" opt
do
   case $opt in
   h) help="true" ;;
   \?) echo "INFO: ignoring unsupported option $OPTARG" ;;
   esac
done

id=$BASH_ARGV

if [ -n "$help" ]; then
   help
   exit 0
fi

(while true; do sleep 60; echo "."; done) &
PONG=$!

rpm -Va --nofiles | sort >&2

kill $PONG &>/dev/null

exit 0

