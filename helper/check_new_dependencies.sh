#!/bin/bash

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 dumps all rpm package versions

EOF
exit 2
fi

MD5=$1
PATCHINFO_URL="http://hilbert.nue.suse.com/abuildstat/patchinfo/$MD5"

for subdir in $(wget -q "$PATCHINFO_URL" -O - | grep DIR | sed -e 's,.*href="\([^"]*\)/">.*,\1,g' | grep -v ^doc$ | grep -v patchinfo$); do
   for package in $(wget -q "$PATCHINFO_URL/$subdir" -O - | grep "\.rpm" | grep -v "delta\.rpm" | sed -e 's,.*href="\([^"]*\)">.*,\1,g'); do
      list="$list $package"
   done
done

echo "list: $list"
if grep -q "VERSION = 11" /etc/SuSE-release; then
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
