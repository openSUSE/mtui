#!/bin/bash

usage="${0##*/} ( before | after ) <result dir> <md5sum>"

mode="$1"
resultdir="$2"
md5sum="$3"

category1="check_new_licenses.sh check_all_updated.pl check_from_same_srcrpm.pl check_vendor_and_disturl.pl run_rpm_Va_nofiles.sh check_multiple-owners.sh"
category2="compare_new_licenses.sh compare_all_updated.sh compare_from_same_srcrpm.sh compare_vendor_and_disturl.pl compare_rpm_Va_nofiles.sh compare_multiple-owners.sh"

mydir="${0%/*}"
PATH="$PATH:$mydir"

if [ -z "$mode" -o -z "$resultdir" -o -z "$md5sum" ]; then
    echo "$usage"
    exit 1
fi

mkdir -p "$resultdir" || exit 1

case "$mode" in
    before)
       for helper in $category1; do
          progname=${helper##*/}
          echo "launching $progname"
          $helper $md5sum 2> $resultdir/$progname.before.err > $resultdir/$progname.before.out
       done
       ;;
    after)
       for helper in $category1; do
          progname=${helper##*/}
          echo "launching $progname"
          $helper $md5sum 2> $resultdir/$progname.after.err > $resultdir/$progname.after.out
       done
       for helper in $category2; do
          progname=${helper##*/}
          checkprog=${progname/check_/compare_}
          checkprog=${checkprog/run_/compare_}
          echo "launching $progname"
          $helper $resultdir/$checkprog.before.err $resultdir/$checkprog.after.err 2> $resultdir/$progname.err > $resultdir/$progname.out
          echo "errors:"
          test -s $resultdir/$progname.err && cat $resultdir/$progname.err || echo "(empty)"
          echo "info:"
          test -s $resultdir/$progname.out && cat $resultdir/$progname.out || echo "(empty)"
       done
      ;; 
    *) echo "$usage" ;;
esac
