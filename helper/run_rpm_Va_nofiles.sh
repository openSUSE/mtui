#!/bin/bash

TEMP=`mktemp /tmp/${0##*/}.XXXXXXXXXX`

rpm -qa | sort -t - -k1,5 | while read p
do 
    echo "INFO: working on $p"
    rpm -V --nofiles $p > $TEMP
    if [ -s $TEMP ]; then
       echo "ERROR: verification of $p revealed:"
       cat $TEMP
    fi 1>&2
done

rm -f $TEMP

exit 0

