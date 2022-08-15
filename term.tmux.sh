#!/bin/sh
HOSTS=$@

TERM_CMD="tmux new-window -n"
for i in $HOSTS; do
   $TERM_CMD $i "ssh -Y root@$i"
done
