#!/bin/sh

export LANG=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> <id>

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 checks if there are any files or symlinks in the file lists of the updated RPMs that are owned by more than one package. Strictly speaking, both cases are errors. However, SUSE tends to reuse symlinks from multiple packages to implement flexibility (like /boot/initrd owned by all installed kernel flavours and /etc/alternatives/* owned by all alternatives). Therefore, only multi-owned files are considered as error. Multi-owned symlinks are printed JFYI.

This check is necessary since rpm -V does not detect such packaging errors and they seldom show up in the update log.

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
