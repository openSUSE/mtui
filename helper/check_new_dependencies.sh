#!/bin/bash

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 -p <path-to-package-list>
$0 dumps all rpm package versions

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

if [ -n "$help" -o -z "$plist" ]; then
   help
   exit 0
fi

# this needs to be sorted for compare_new_dependencies.sh
# to work properly
list=$(
    sed -n '/\.delta\.\(log\|info\|rpm\)/! { /\.rpm$/p; }' $plist | while read p
    do
       pn=${p%-[^-]*-[^-]*\.[^.]*\.rpm}
       echo ${pn##*/}
    done | sort -u | xargs
)

# `rpm -q --requires` outputs lines in these formats:
# * package dependency
# * shared lib dependency
# * path
# * capability line
# * error message about uninstalled package
#
# examples:
#   /usr/bin/python
#   python-m2crypto > 0.19
#   python-urlgrabber
#   config(osc) = 0.152.0-1.1
#   libdl.so.2(GLIBC_2.2.5)(64bit)
#   libreadline.so.6()(64bit)
#   package fubar is not installed
#
# output is *mostly* sorted and without duplicates, but see this:
#
#  edna:~ # rpm -q --requires gawk
#  /bin/sh
#  /bin/sh
#  /bin/sh
#  /bin/sh
#  info
#  libc.so.6()(64bit)
#  [...]
#  rpmlib(PayloadFilesHavePrefix) <= 4.0-1
#  update-alternatives
#  rpmlib(PayloadIsLzma) <= 4.4.6-1

for p in $list; do
  rpm -q --requires $p \
  | sort -u \
  | sed "
      /is not installed/d
      # a line with spaces should be a line in the form CAP OP VER,
      # we're only doing loose comparisons so strip the latter two
      s/ .*//
      # label each dependency with the dependent package so
      # compare_new_dependencies.sh knows what it's looking at and
      # can present to user which package is affected by each change.
      s/^/$p /
    "
done
