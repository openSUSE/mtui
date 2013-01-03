#!/bin/bash

progname=${0##*/}

if ! [ -f "$1" -a -f "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

newmismatches=$(diff -Nur "$1" "$2" | grep -E "^\+(ERROR:|\s*[[:xdigit:]]{32,})")
gonemismatches=$(diff -Nur "$1" "$2" | grep -E "^\-(ERROR:|\s*[[:xdigit:]]{32,})")

if [ -n "$newmismatches" ]; then
   (
   echo "ERROR: found new (or different) errors after update:"
   cat <<EOF
$newmismatches
EOF
   #echo $newmismatches
   ) >&2 
   exit 1
fi

if [ -n "$gonemismatches" ]; then
   echo "INFO: good, some errors disappeared after update:"
   cat <<EOF
$gonemismatches
EOF
   exit 0
fi

