#!/bin/sh
HOSTS=$@

for i in $HOSTS; do
	para="$para --tab -e \"ssh -X root@$i\""
done

eval "gnome-terminal $para" &>/dev/null &
