#!/bin/bash

export LC_ALL=C

progname=${0##*/}

if [ -z "$1" -o -z "$2" ]; then
   echo "usage: $progname <dependencies before the update> <dependencies after the update>"
   exit 2
fi


if cmp -s "$1" "$2"; then
  echo "no deps changed"
  exit 0
fi

{
  echo "ERROR: dropped dependencies:"
  comm -23 "$1" "$2"
  echo "ERROR: added dependencies:"
  comm -13 "$1" "$2"
  exit 1
} >&2

