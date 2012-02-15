#!/bin/bash

if [ "$1" = "-h" ]; then
cat <<EOF

$0 takes no arguments

$0 dumps all rpm package licenses 

EOF
exit 0
fi

rpm -aq --queryformat '%{LICENSE}\n' | sort -u
