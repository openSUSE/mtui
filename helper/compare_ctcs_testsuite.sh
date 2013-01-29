#!/bin/bash

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

# strip colors from output
sed -i -e 's/\x1b\[[0-9]*m//g' "$1" "$2"

before=$(grep -A 1 "^Tests passed:" $1 | tail -n 1)
after=$(grep -A 1 "^Tests passed:" $2 | tail -n 1)

if [ -z "$before" ]; then
    # no succeeded testcases prior to the update found, finding
    # regressions not possible. exit with NOT RUN
    exit 3
fi

for testcase in $after; do
    before=${before/$testcase /}
done

before=$(echo $before)

if [ ! -z "$before" ]; then
    echo "CTCS2 testcase regressions: $before"
    exit 1
else
    exit 0
fi
