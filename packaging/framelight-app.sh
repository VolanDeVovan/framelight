#!/bin/sh
# The framelight switch, started by framelight.desktop from the headset's app list.
# It shows one button (Enable / Disable) and the address to open in a browser.
cd "$(dirname "$0")" || exit 1
# Keep the previous run's log, and no more than that.
[ -f app.log ] && mv -f app.log app.log.1
exec ./framelight app "$@" >app.log 2>&1
