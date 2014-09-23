#!/bin/sh

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> [id]

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 checks if there are any files or symlinks in the file lists of the updated RPMs that are owned by more than one package. Strictly speaking, both cases are errors. However, SUSE tends to reuse symlinks from multiple packages to implement flexibility (like /boot/initrd owned by all installed kernel flavours and /etc/alternatives/* owned by all alternatives). Therefore, only multi-owned files are considered as error. Multi-owned symlinks are printed JFYI.

This check is necessary since rpm -V does not detect such packaging errors and they seldom show up in the update log.

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

for package in $list
do
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
