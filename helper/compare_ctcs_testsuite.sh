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

temp1=$(mktemp /tmp/$progname.XXXXXX)
echo $before | xargs -n 1 | sort > $temp1

temp2=$(mktemp /tmp/$progname.XXXXXX)
echo $after | xargs -n 1 | sort > $temp2

trap "rm -f $temp1 $temp2" SIGINT SIGKILL EXIT

if [ -z "$before" ]; then
    # no succeeded testcases prior to the update found, finding
    # regressions not possible. exit with NOT RUN
    exit 3
fi

regression=$(comm -23 $temp1 $temp2 | xargs)

if [ ! -z "$regression" ]; then
    echo "CTCS2 testcase regressions: $regression"
    exit 1
else
    exit 0
fi
