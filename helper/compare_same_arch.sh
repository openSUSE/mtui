#!/bin/bash

progname=${0##*/}

if ! [ -f "$1" -a -f "$2" ]; then
   echo "usage: $progname <logfile before the update> <logfile after the update>"
   exit 2
fi

pre=$1
post=$2
mismatch=0

for package in $(sed -e 's,:.*,,g' $pre); do
    if [ "$(grep $package $pre)" != "$(grep $package $post)" ]; then
        mismatch=1
        grep -h "$package:" $pre $post
    fi
done

if [ $mismatch -eq 1 ]; then
   echo "ERROR: package arch was changed"
   exit 1
fi

exit 0

