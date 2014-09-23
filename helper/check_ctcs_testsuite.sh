#!/bin/bash

export LC_ALL=C

help () {
   cat <<EOF

usage: $0 -p <path-to-filelist> [id]

where id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

$0 runs ctcs testsuites if a matching package is found

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

blacklist="qa_test_lvm2"

zypper -n ref >/dev/null 2>&1
if grep -Eq "VERSION = (11|12)" /etc/SuSE-release; then
    testsuites=$(zypper se -s qa_test -t package 2>/dev/null | grep qa_test | awk -F\| '{sub(/ qa_test_/,""); print $2 }' | sort -u)
elif grep -q "VERSION = 10" /etc/SuSE-release; then
    testsuites=$(zypper se qa_test 2>/dev/null | grep qa_test | awk -F\| '{sub(/ qa_test_/,""); print $4 }' | sort -u)
fi

# remove blacklisted testsuites
for testsuite in $blacklist; do
    testsuites=${testsuites/$testsuite/}
done

for testsuite in $testsuites; do
    if [[ $list =~ $testsuite ]]; then
        zypper -n in qa_test_$testsuite qa_lib_ctcs2 qa_tools >/dev/null 2>&1
        if [ -x /usr/share/qa/tools/test_$testsuite-run ]; then
            export TESTS_LOGDIR=/var/log/qa/$id
            /usr/share/qa/tools/test_$testsuite-run
        fi
    fi
done
