#!/bin/bash

(while true; do sleep 60; echo "."; done) &
PONG=$!

rpm -Va --nofiles | sort

kill $PONG &>/dev/null

exit 0

