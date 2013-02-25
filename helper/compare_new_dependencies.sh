#!/bin/bash

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

list=$(grep "list:" "$1" | sed 's,list: ,,g')
#changed=$(diff -Nur "$1" "$2" | grep "^\+[[:alpha:]]" | cut -d: -f1 | sed 's,^+,,g')
changed=$(diff -Nur "$1" "$2" | grep "^\+[[:alpha:]]" | sed 's,^+,,g')

for package in $changed; do
   package=$(echo $package | sed 's,-[^-]*-[^-]*$,,g')
   if ! [[ "$list " =~ "$package " ]]; then
      new="$new $package"
   fi
done

if [ -z "$new" ]; then
   echo "no external packages affected"
   exit 0
else
   echo "affected external packages: $new"
   exit 1
fi

