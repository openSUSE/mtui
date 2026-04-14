#!/bin/bash

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
    echo "usage: $progname <logfile before the update> <logfile after the update>"
    # INTERNAL ERROR
    exit 2
fi

if grep -q "no initrd found" $2; then
    # SKIPPED
    exit 3
fi

regression=$(comm -13 $1 $2)

if [ ! -z "$regression" ]; then
    echo "possibly outdated /boot/initrd files:"
    echo $regression | xargs -n1
    # ERROR
    exit 1
else
    # OK
    exit 0
fi
