#!/bin/sh
# Compares mxbot-common's expansion of .ics files with recurring-ical-events' (oracle.py):
# the same "local start|title" lines, or the difference. Needs python3 (a venv is made in
# tools/ics-oracle/.venv on first use, from requirements.txt).
#
#   tools/ics-oracle/compare.sh [files…]     default: src/calendar/testdata/*.ics
#   FROM=2026-10-01 DAYS=150 ZONE=Europe/Berlin
#
# Known difference (intended): an RDATE;VALUE=DATE of a timed series is all day here and
# midnight there.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
from=${FROM:-2026-10-01}
days=${DAYS:-150}
zone=${ZONE:-Europe/Berlin}
venv="$here/.venv"
if [ ! -x "$venv/bin/python" ]; then
  python3 -m venv "$venv"
  "$venv/bin/pip" install -q --disable-pip-version-check -r "$here/requirements.txt"
fi
(cd "$root" && cargo build -q --features calendar --example ics_listing)
listing="$root/target/debug/examples/ics_listing"
[ $# -gt 0 ] || set -- "$root"/src/calendar/testdata/*.ics
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
status=0
for f in "$@"; do
  "$venv/bin/python" -I "$here/oracle.py" "$f" "$from" "$days" "$zone" > "$tmp/py"
  "$listing" "$f" "$from" "$days" "$zone" > "$tmp/rs"
  if cmp -s "$tmp/py" "$tmp/rs"; then
    echo "same       $(basename "$f") ($(wc -l < "$tmp/rs") occurrences)"
  else
    echo "DIFFERENT  $(basename "$f")   (< recurring-ical-events, > mxbot-common)"
    diff "$tmp/py" "$tmp/rs" | sed 's/^/    /' || true
    status=1
  fi
done
exit $status
