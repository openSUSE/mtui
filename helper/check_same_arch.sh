#!/bin/bash

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 dumps all rpm package architectures

EOF
exit 0
fi

MD5=$1
PATCHINFO_URL="http://hilbert.nue.suse.com/abuildstat/patchinfo/$MD5/patchinfo"

list=$(wget -q $PATCHINFO_URL -O - | grep " release " | cut -d " " -f 1 | sort -u | xargs)

for package in $list; do
   rpm -q $package &>/dev/null && rpm -q --queryformat '%{NAME}: %{ARCH}\n' $package | uniq
done
