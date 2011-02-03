#!/bin/bash

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

temp1=$(mktemp /tmp/$progname.XXXXXX)
grep -E "^ERROR" "$1"  > $temp1
num1=$(wc -l $temp1 | cut -d ' ' -f 1)

temp2=$(mktemp /tmp/$progname.XXXXXX)
grep -E "^ERROR" "$2"  > $temp2
num2=$(wc -l $temp2 | cut -d ' ' -f 1)

trap "rm -f $temp1 $temp2" SIGINT SIGKILL EXIT

newerrors=$(diff -Nur "$temp1" "$temp2" | grep -E "^\+ERROR")
goneerrors=$(diff -Nur "$temp1" "$temp2" | grep -E "^\-ERROR")

if [ -n "$newerrors" ]; then
   (
   echo "ERROR: found new errors after update (before:$num1 vs after:$num2):" 
   #diff -Nur "$1" "$2"
   diff -Nur "$temp1" "$temp2" | grep -E "^\+ERROR"
   ) >&2 
   exit 1
fi

if [ -n "$goneerrors" ]; then
   echo "INFO: good, some errors disappeared after update (before:$num1 vs after:$num2):"
   #diff -Nur "$1" "$2"
   diff -Nur "$temp1" "$temp2" | grep -E "^\-ERROR"
   exit 0
fi

