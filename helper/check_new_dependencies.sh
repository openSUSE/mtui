#!/bin/bash

export LANG=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> <id>

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 dumps all rpm package versions

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
