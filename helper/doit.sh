#!/bin/bash

usage="${0##*/} ( before | after ) <result dir> <md5sum>"

mode="$1"
resultdir="$2"
md5sum="$3"

category1="check_new_dependencies.sh check_all_updated.pl check_dependencies.sh check_from_same_srcrpm.pl check_multiple-owners.sh check_new_licenses.sh check_vendor_and_disturl.pl check_same_arch.sh check_ctcs_testsuite.sh check_initrd_state.sh"
category2="compare_new_dependencies.sh compare_all_updated.sh compare_dependencies.sh compare_from_same_srcrpm.sh compare_multiple-owners.sh compare_new_licenses.sh compare_rpm_Va.sh compare_vendor_and_disturl.sh compare_same_arch.sh compare_ctcs_testsuite.sh compare_initrd_state.sh"

mydir="${0%/*}"
PATH="$PATH:$mydir"

declare -a results

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
       i=0
       for helper in $category2; do
          progname=${helper##*/}
          checkprog=${progname/compare_/check_}
          echo "launching $progname"
          $helper $resultdir/${checkprog%.*}*.before.err $resultdir/${checkprog%.*}*.after.err 2> $resultdir/$progname.err > $resultdir/$progname.out
          resultname=${progname%.*}
          echo "errors:"
          if  [ -s $resultdir/$progname.err ]; then
             results[$i]="FAILED"
             i=$[ $i + 1 ]
             cat $resultdir/$progname.err
          else 
             results[$i]="PASSED"
             i=$[ $i + 1 ]
             result="$result`echo -n \t$resultname\t: PASSED\n`"
             echo "(empty)"
          fi
            
          echo "info:"
          if [ -s $resultdir/$progname.out ]; then
              cat $resultdir/$progname.out 
          else
             echo "(empty)"
          fi
       done
       i=0
       for helper in $category1; do
          resultname=${helper##*/}
          resultname=${resultname%.*}
          echo "$resultname : ${results[$i]}"
          i=$[ $i + 1 ]
       done | column -t
      ;; 
    *) echo "$usage" ;;
esac

