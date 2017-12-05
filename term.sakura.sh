#!/bin/bash
HOSTS=$@

TERM_CMD="sakura"
for i in $HOSTS; do
   $TERM_CMD -t $i -x "ssh -Y root@$i" &> /dev/null &
done
