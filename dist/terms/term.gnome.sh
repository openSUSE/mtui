#!/bin/sh
HOSTS=$@

para="$GNOME_TERMINAL_PARAMETERS"
for i in $HOSTS; do
        HF=/tmp/mtui.bashhistfile.$i
        test -r "$HISTFILE" && cp $HISTFILE $HF
        echo "ssh -Y root@$i" >> $HF
        RCF=/tmp/mtui.rcfile.$i
        echo 'test -s ~/.bashrc && . ~/.bashrc' > $RCF
        echo "export HISTFILE=$HF" >> $RCF
        echo "ssh -Y root@$i" >> $RCF
        para="$para --tab -e \"bash --rcfile $RCF\""
        # para="$para --tab -e \"ssh -X root@$i\""
done

echo "gnome-terminal $para"
eval "gnome-terminal $para" &>/dev/null &
