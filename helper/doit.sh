#!/bin/bash

usage="$0 -m (before|after) -o <result dir> -r <repo> -p <file-with-package-list> <id>"

ARGS=$(getopt -o m:o:r:p:h -- "$@")

eval set -- "$ARGS"

while true; do
   case "$1" in
      -m) shift; mode="$1"; shift; ;;
      -o) shift; resultdir="$1"; shift; ;;
      -r) shift; repo="$1"; shift; ;;
      -p) shift; plist="$1"; shift; ;;
      -h) shift; help="set" ;;
      --) shift; break; ;;
   esac
done

id="$1"

if [ $? -gt 0 ]; then echo "$usage"; exit 1; fi

if [ $mode != "before" -a $mode != "after" ]; then
   echo $usage
   exit 1
fi

if [ -z "$resultdir" -o -z "$repo" -o -z "$plist" -o -z "$id" ]; then
   echo "$usage"
   exit 1
fi

if [ "${plist#http://}" != "$plist" ]; then
   temp_plist=$(mktemp /tmp/plist.XXXXXX)
   trap "rm -f $temp_plist" SIGINT SIGKILL EXIT
   curl -s $plist > $temp_plist
   plist=$temp_plist
fi

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

declare -a results

PATH="$PATH:${0%/*}"

mkdir -p "$resultdir" || exit 1

function run-script
{
   local mode=$1 script=${2%%:*} compare=${2##*:}
   local progname=${script##*/}
   local comparename=${compare##*/}

   echo "launching $script $mode update"
   $script -r $repo -p $plist $id 2> $resultdir/$progname.$mode.err > $resultdir/$progname.$mode.out

   if [ $mode != "before" ]; then
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
   fi
}

for item in ${scripts[*]}; do
   run-script $mode $item
done

for result in $results; do 
   name=${result%:*}
   outcome=${result#*:}
   echo -e "\t$name\t$outcome"
done | column -t

