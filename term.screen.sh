#!/bin/sh
HOSTS=$@

TERM_CMD="screen" 

for i in ${HOSTS}
   do
   $TERM_CMD -t ${i} ssh -Y root@${i}
done
