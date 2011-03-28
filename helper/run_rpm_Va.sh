#!/bin/bash

# typical run times:
# frisch (sles11sp1): 19m
# vmware guest (sles10sp4): 4m
# boxer (sles10sp3): 8m
# dax (sles9sp3): 5m

if [ "$1" = "-h" ]; then
cat <<'EOF'
output of the rpm --verify command (excerpt from man page):

       The format of the output is a string of 8 characters, a possible attribute marker:

       c %config configuration file.
       d %doc documentation file.
       g %ghost file (i.e. the file contents are not included in the package payload).
       l %license license file.
       r %readme readme file.

       from the package header, followed by the file name.  Each of the 8 characters denotes the result of  a  comparison
       of  attribute(s)  of  the file to the value of those attribute(s) recorded in the database.  A single "." (period)
       means the test passed, while a single "?" (question mark) indicates the test could not  be  performed  (e.g.  file
       permissions  prevent  reading).  Otherwise,  the (mnemonically emBoldened) character denotes failure of the corre-
       sponding --verify test:

       S file Size differs
       M Mode differs (includes permissions and file type)
       5 MD5 sum differs
       D Device major/minor number mismatch
       L readLink(2) path mismatch
       U User ownership differs
       G Group ownership differs
       T mTime differs
       P caPabilities differ

EOF
exit 0
fi

TEMP=`mktemp /tmp/${0##*/}.XXXXXXXXXX`

rpm -qa | sort -t - -k1,5 | while read p
do 
    echo "INFO: working on $p"
    rpm -V $p > $TEMP
    if [ -s $TEMP ]; then
       echo "ERROR: verification of $p revealed:"
       cat $TEMP
    fi 1>&2
done

rm -f $TEMP

exit 0

