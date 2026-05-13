#!/bin/zsh
set -euo pipefail

BASE_URL="${PAVLOV_URL:-http://127.0.0.1:8765}"
INTENSITY="${PAVLOV_WAKE_INTENSITY:-50}"
ENTRIES="${PAVLOV_WAKE_ENTRIES:-6}"
NAME="${PAVLOV_WAKE_NAME:-Wake}"
MODE="${1:-normal}"

if [[ "$MODE" == "--test" || "$MODE" == "test" ]]; then
  minutes=2
  entries=1
  name="WakeTest"
else
  today=$(/bin/date +%Y-%m-%d)
  target=$(/bin/date -j -f "%Y-%m-%d %H:%M:%S" "${today} 09:05:00" +%s)
  now=$(/bin/date +%s)
  if (( target <= now )); then
    target=$(/bin/date -j -v+1d -f "%Y-%m-%d %H:%M:%S" "${today} 09:05:00" +%s)
  fi
  minutes=$(( (target - now + 59) / 60 ))
  entries="$ENTRIES"
  name="$NAME"
fi

encoded_name=$(/usr/bin/python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$name")

/usr/bin/curl -fsS -X POST \
  "${BASE_URL}/alarm/wakeup/schedule?minutes=${minutes}&entries=${entries}&intensity=${INTENSITY}&name=${encoded_name}"
