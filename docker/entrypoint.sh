#!/bin/sh
# Puts a writable yt-dlp in /config on first start so Lantern's daily
# `yt-dlp -U` can update it there; the copy survives image updates.
set -e
DIR="${LANTERN_DIR:-/config}"
mkdir -p "$DIR"
if [ ! -x "$DIR/yt-dlp" ]; then
  cp /usr/local/share/lantern/yt-dlp "$DIR/yt-dlp"
  chmod 755 "$DIR/yt-dlp"
fi
exec /usr/local/bin/lantern-server "$@"
