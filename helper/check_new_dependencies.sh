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
   for package in $(wget -q "$PATCHINFO_URL/$subdir" -O - | grep rpm | grep -v delta | sed -e 's,.*href="\([^"]*\)">.*,\1,g'); do
      list="$list $package"
   done
done

echo "list: $list"
rpm -qa --queryformat '%{NAME}: %{VERSION}-%{RELEASE}\n' | sort -u
