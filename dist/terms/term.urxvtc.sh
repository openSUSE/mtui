#!/bin/bash
HOSTS=$@

TERM_CMD="urxvtc-256color"
for i in $HOSTS; do
    $TERM_CMD -e ssh -Y root@$i &
done
