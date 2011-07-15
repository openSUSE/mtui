#!/bin/bash

if [ "$1" = "-h" ]; then
cat <<EOF

$0 takes no arguments

$0 checks if there are any files or symlinks in the file lists of the installed RPMs that are owned by more than package. Strictly speaking, both cases are errors. However, SUSE tends to reuse symlinks from multiple packages to implement flexibility (like /boot/initrd owned by all installed kernel flavours and /etc/alternatives/* owned by all alternatives). Therefore, only multi-owned files are considered as error. Multi-owned symlinks are printed JFYI.

This check is necessary since rpm -V does not detect such packaging errors and they seldom show up in the update log.

EOF
exit 0
fi

rpm -qal | 
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
      fi 
      
   fi
done

