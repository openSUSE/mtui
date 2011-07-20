#!/bin/sh
HOSTS=$@

TERM_CMD="xterm -bg black -fg white"
for i in $HOSTS; do
   $TERM_CMD -T $i -e ssh -Y root@$i &
done
