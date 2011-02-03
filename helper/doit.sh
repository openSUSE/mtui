#!/bin/bash

usage="${0##*/} ( before | after ) <result dir>"

mode="$1"
resultdir="$2"

category1="check_all_updated.pl check_from_same_srcrpm.pl check_vendor_and_disturl.pl"
category2="compare_vendor_and_disturl.pl compare_all_updated.sh compare_from_same_srcrpm.sh"

mydir="${0%/*}"
PATH="$PATH:$mydir"

if [ -z "$mode" -o -z "$resultdir" ]; then
    echo "$usage"
    exit 1
fi

mkdir -p "$resultdir" || exit 1

case "$mode" in
    before)
       for helper in $category1; do
          progname=${helper##*/}
          echo "launching $progname"
          if [ "$progname" = "check_all_updated.pl" ]; then helper="$helper --installed"; fi
          $helper 2> $resultdir/$progname.before.error > $resultdir/$progname.before.out
       done
       ;;
    after)
       for helper in $category1; do
          progname=${helper##*/}
          echo "launching $progname"
          if [ "$progname" = "check_all_updated.pl" ]; then helper="$helper --installed"; fi
          $helper 2> $resultdir/$progname.after.error > $resultdir/$progname.after.out
       done
       for helper in $category2; do
          progname=${helper##*/}
          checkprog=${progname/check_/compare_}
          echo "launching $progname"
          $helper $resultdir/$checkprog.before.error $resultdir/$checkprog.after.error 2> $resultdir/$progname.errors > $resultdir/$progname.out
       done
      ;; 
    *) echo "$usage" ;;
esac
