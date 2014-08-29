#!/bin/bash

usage="$0 -m (before|after) -o <result dir> -r <repo> -p <file-with-package-list> <id>"

while getopts "m:o:r:p:h" opt
do
   case $opt in
   m) mode="$OPTARG" ;;
   o) resultdir="$OPTARG" ;;
   r) repo="$OPTARG" ;;
   p) plist="$OPTARG" ;;
   h) help="true" ;;
   \?) exit 1 ;;
   esac
done

declare -a scripts=(
   check_new_dependencies.sh:compare_new_dependencies.sh
   check_all_updated.pl:compare_all_updated.sh
   check_dependencies.sh:compare_dependencies.sh
   check_from_same_srcrpm.pl:compare_from_same_srcrpm.sh
   check_multiple-owners.sh:compare_multiple-owners.sh
   check_new_licenses.sh:compare_new_licenses.sh
   check_vendor_and_disturl.pl:compare_vendor_and_disturl.sh
   check_same_arch.sh:compare_same_arch.sh
   check_ctcs_testsuite.sh:compare_ctcs_testsuite.sh
   check_initrd_state.sh:compare_initrd_state.sh
)

PATH="$PATH:${0%/*}"

id=$BASH_ARGV

declare -a results

if [ -z "$mode" -o -z "$resultdir" -o -z "$repo" -o -z "$plist" -o -z "$id" ]; then
    echo "$usage"
    exit 1
fi

mkdir -p "$resultdir" || exit 1

case "$mode" in
    before)
       for item in ${scripts[*]}; do
          script=${item%%:*}
          progname=${script##*/}
          echo "launching $script $mode update"
          $script -r $repo -p $plist $id 2> $resultdir/$progname.before.err > $resultdir/$progname.before.out
       done
       ;;
    after)
       for item in ${scripts[*]}; do
          script=${item%%:*}
          progname=${script##*/}
          echo "launching $script $mode update"
          $script -r $repo -p $plist $id 2> $resultdir/$progname.after.err > $resultdir/$progname.after.out

          compare=${item##*:}
          comparename=${compare##*/}
          echo "launching $compare"
          $compare $resultdir/$progname.before.err $resultdir/$progname.after.err 2> $resultdir/$comparename.err > $resultdir/$comparename.out

          echo "errors:"
          if  [ -s $resultdir/$comparename.err ]; then
             result="${progname%.*}:FAILED"
             cat $resultdir/$comparename.err
          else 
             result="${progname%.*}:PASSED"
             echo "(empty)"
          fi

          echo "info:"
          if [ -s $resultdir/$comparename.out ]; then
              cat $resultdir/$comparename.out 
          else
             echo "(empty)"
          fi

          echo ""
          if [ -z "$results" ]; then
             results="$result"
          else
             results="$results $result"
          fi
       done

       for result in $results; do 
          name=${result%:*}
          outcome=${result#*:}
          echo -e "\t$name\t$outcome"
       done | column -t
       ;; 

    *) echo "$usage" ;;
esac

