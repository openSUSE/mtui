#!/bin/bash

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 compares initrd files to system files

EOF
exit 2
fi

if [ ! -f "/boot/initrd" ]; then
    echo "no initrd found"
    exit 3
fi

MD5=$1
PATCHINFO_URL="http://hilbert.nue.suse.com/abuildstat/patchinfo/$MD5/patchinfo"

list=$(wget -q $PATCHINFO_URL -O - | grep " release " | cut -d " " -f 1 | sort -u | xargs)

dir=$(mktemp -d /tmp/$progname.XXXXXX)

cd $dir && gunzip < /boot/initrd | cpio -i --make-directories > /dev/null 2>&1 

# try to find files which might end up in /boot/initrd
for package in $list; do
    for file in $(rpm -ql $package | sort); do
        if [ -f ".$file" ]; then
            if [ "$(md5sum "$file" | awk '{ print $1 }')" != "$(md5sum ".$file" | awk '{ print $1 }')" ]; then
                echo $file
            fi
        fi
    done
done

rm -rf $dir
