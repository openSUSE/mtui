#!/bin/bash

TEMP_WINDOW='/Windows/1'
: ${KONSOLE_DBUS_WINDOW:=${TEMP_WINDOW}}

while [ $# -gt 0 ] ; do
        session_num=$(qdbus-qt5 $KONSOLE_DBUS_SERVICE $KONSOLE_DBUS_WINDOW  newSession)
        sleep 1
        qdbus-qt5  $KONSOLE_DBUS_SERVICE /Sessions/${session_num} runCommand "ssh -Y root@${1}" > /dev/null
        qdbus-qt5  $KONSOLE_DBUS_SERVICE /Sessions/${session_num} setTitle 1 "${1}" > /dev/null
        shift
        sleep 1
done
