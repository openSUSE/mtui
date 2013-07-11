#!/bin/bash

if [ -z "$1" -o "$1" = "-h" ]; then
cat <<EOF

$0 <md5sum>

$0 runs ctcs testsuites if a matching package is found

EOF
exit 2
fi

MD5=$1
PATCHINFO_URL="http://hilbert.nue.suse.com/abuildstat/patchinfo/$MD5/patchinfo"

blacklist="qa_test_lvm2"

list=$(wget -q $PATCHINFO_URL -O - | grep " release " | cut -d " " -f 1 | sort -u | xargs)

zypper -n ref >/dev/null 2>&1
if grep -q "VERSION = 11" /etc/SuSE-release; then
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
        zypper -n in qa_test_$testsuite qa_lib_ctcs2 >/dev/null 2>&1
        if [ -x /usr/share/qa/tools/test_$testsuite-run ]; then
            export TESTS_LOGDIR=/var/log/qa/$MD5
            /usr/share/qa/tools/test_$testsuite-run
        fi
    fi
done
