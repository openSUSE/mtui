#!/bin/sh

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 checks if there are any files or symlinks in the file lists of the updated RPMs that are owned by more than one package. Strictly speaking, both cases are errors. However, SUSE tends to reuse symlinks from multiple packages to implement flexibility (like /boot/initrd owned by all installed kernel flavours and /etc/alternatives/* owned by all alternatives). Therefore, only multi-owned files are considered as error. Multi-owned symlinks are printed JFYI.

This check is necessary since rpm -V does not detect such packaging errors and they seldom show up in the update log.

EOF
exit 0
fi

MD5=$1
PATCHINFO_URL="http://hilbert.suse.de/abuildstat/patchinfo/$MD5"

list=""

for subdir in $(wget -q "$PATCHINFO_URL" -O - | grep DIR | sed -e 's,.*href="\([^"]*\)/">.*,\1,g' | grep -v ^doc$ | grep -v patchinfo$); do 
   for package in $(wget -q "$PATCHINFO_URL/$subdir" -O - | grep rpm | grep -v delta | sed -e 's,.*href="\([^"]*\)">.*,\1,g'); do
      list="$list $package"
   done
done

for package in $(echo $list | tr " " "\n" | sed -e 's,\(.\+\)-[^-]\+-[^-]\+\.\w\+\.rpm,\1,g' | sort -u); do
   rpm -ql $package | 
   grep -v "(contains no files)" | 
   sort -t / | 
   uniq -c | 
   while read count path; do 
      if [ $count -gt 1 ]; then 
         if [ ! -d $path ];  then 
            owners=`rpm -qf --qf "%{NAME}\n" $path | sort -t - | tr [:cntrl:] ' '`
            if [ ! -L $path ]; then
               echo "ERROR: file $path owned by ($owners)" 1>&2
            else
               echo "INFO: symlink $path owned by ($owners)"
            fi
            rpm -Va $owners
         fi 
      fi
   done
done
