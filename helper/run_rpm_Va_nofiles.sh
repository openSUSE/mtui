#!/bin/bash

TEMP=`mktemp /tmp/${0##*/}.XXXXXXXXXX`
count=0

rpm -qa | sort -t - -k1,5 | while read p
do 
    if [ $[ count % 100 ] -eq 0 ]; then echo -n "."; fi
    count=$[ $count + 1 ]

    rpm -V --nofiles $p > $TEMP
    if [ -s $TEMP ]; then
       echo -e "\nERROR: verification of $p revealed:"
       cat $TEMP
    fi 1>&2
done

rm -f $TEMP

exit 0

