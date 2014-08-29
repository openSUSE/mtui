#!/bin/bash

export LANG=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> <id>

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 dumps all rpm package licenses 

EOF
}

while getopts ":r:p:h" opt
do
   case $opt in
   p) plist="$OPTARG" ;;
   h) help="true" ;;
   \?) echo "ERROR: unsupported option $OPTARG" >&2; exit 1 ;;
   esac
done

id=$BASH_ARGV

if [ -n "$help" -o -z "$plist" ]; then
   help
   exit 0
fi

list=$(
    grep -Ev "\.delta\.(log|info|rpm)" $plist | grep -E "\.rpm$" | while read p
    do
       pn=${p%-[^-]*-[^-]*\.[^.]*\.rpm}
       echo ${pn##*/}
    done | sort -u | xargs
)

for package in $list; do
   rpm -q $package &>/dev/null && rpm -q --queryformat '%{NAME}: %{LICENSE}\n' $package
done
