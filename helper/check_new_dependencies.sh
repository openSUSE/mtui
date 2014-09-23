#!/bin/bash

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> [id]

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 dumps all rpm package versions

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

echo "list: \"$list\""
if grep -Eq "VERSION = (11|12)" /etc/SuSE-release; then
    for repo in $(zypper lr -s | awk -F\| '{ print $2 }'); do
        if [[ "$repo" =~ "TESTING" ]]; then
            zypper -n mr -d $repo >/dev/null 2>&1
        elif [[ "$repo" =~ "UPDATE" ]]; then
            zypper -n mr -e $repo >/dev/null 2>&1
        fi
    done
    zypper -n se -t package -is 2>/dev/null | egrep '(TESTING|System)' | awk -F\| '{gsub(/[ \t]+/,""); print $2"-"$4 }' | sort -u
elif grep -q "VERSION = 10" /etc/SuSE-release; then
    for repo in /var/lib/zypp/db/sources/*; do
        if grep -q TESTING $repo; then
            sed -i -e 's,<enabled>.*</enabled>,<enabled>false</enabled>,g' $repo
	elif grep -q UPDATE $repo; then
            sed -i -e 's,<enabled>.*</enabled>,<enabled>true</enabled>,g' $repo
        fi
    done
    zypper -n se -t package -i 2>/dev/null | egrep '(TESTING|System)' | awk -F\| '{gsub(/[ \t]+/,""); print $4"-"$5 }' | sort -u
else
    rpm -qa --queryformat '%{NAME}-%{VERSION}-%{RELEASE}\n' | sort -u
fi
