#!/bin/bash

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> [id]

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 dumps all rpm package architectures

EOF
}

ARGS=$(getopt -o p:r:h -- "$@")

if [ $? -gt 0 ]; then help; exit 1; fi

eval set -- "$ARGS"

while true; do
   case "$1" in
      -p) shift; plist="$1"; shift; ;;
      -r) shift; echo "INFO: option -r is not implemented"; shift; ;;
      -h) shift; help="set" ;;
      --) shift; break; ;;
   esac
done

id="$1"

if [ -n "$help" -o -z "$plist" ]; then
   help
   exit 0
fi

list=$(
    sed -n '/\.delta\.\(log\|info\|rpm\)/! { /\.rpm$/p; }' $plist | while read p
    do
       pn=${p%-[^-]*-[^-]*\.[^.]*\.rpm}
       echo ${pn##*/}
    done | sort -u | xargs
)

for package in $list; do
   rpm -q $package &>/dev/null && rpm -q --queryformat '%{NAME}: %{ARCH}\n' $package | uniq
done
