#!/bin/sh
# The framelight switch: add THIS file to Steam as a non-Steam game and open it on the
# headset. It shows one button (Enable / Disable) and the address to open in a browser.
cd "$(dirname "$0")" || exit 1
exec ./framelight app "$@" >>app.log 2>&1
