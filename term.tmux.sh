#!/bin/sh
HOSTS=$@

TERM_CMD="tmux new-window"
for i in $HOSTS; do
   $TERM_CMD "ssh -Y root@$i"
done
