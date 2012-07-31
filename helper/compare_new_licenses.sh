#!/bin/bash

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

temp=$(mktemp /tmp/$progname.XXXXXX)
diff -Naur "$1" "$2" > $temp

if [ -s $temp ]; then
   (
   echo "ERROR: found new rpm license texts (before:$1 vs after:$2):"
   cat $temp 
   ) >&2 
   rm $temp
   exit 1
fi

rm $temp
exit 0

