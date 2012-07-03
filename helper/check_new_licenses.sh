#!/bin/bash

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 dumps all rpm package licenses 

EOF
exit 0
fi

MD5=$1
PATCHINFO_URL="http://hilbert.nue.suse.com/abuildstat/patchinfo/$MD5"

list=""

for subdir in $(wget -q "$PATCHINFO_URL" -O - | grep DIR | sed -e 's,.*href="\([^"]*\)/">.*,\1,g' | grep -v ^doc$ | grep -v patchinfo$); do 
   for package in $(wget -q "$PATCHINFO_URL/$subdir" -O - | grep rpm | grep -v delta | sed -e 's,.*href="\([^"]*\)">.*,\1,g'); do
      list="$list $package"
   done
done

for package in $(echo $list | tr " " "\n" | sed -e 's,\(.\+\)-[^-]\+-[^-]\+\.\w\+\.rpm,\1,g' | sort -u); do
   rpm -q $package &>/dev/null && rpm -q --queryformat '%{NAME}: %{LICENSE}\n' $package
done
