#!/bin/sh
# Fetches this month's DB-IP IP to Country Lite database (CC BY 4.0) into /opt/puntohunter/geo, for [geo] in
# the pool's config. Run it monthly, from a systemd timer or cron. The file is replaced in one rename, so the
# pool, which checks it hourly, never reads half a file; a failed fetch leaves the last good copy in place.
set -eu
dir=/opt/puntohunter/geo
month=$(date -u +%Y-%m)
tmp=$(mktemp "$dir/.dbip-XXXXXX")
trap 'rm -f "$tmp" "$tmp.gz"' EXIT
curl --fail --silent --show-error --location --max-time 300 \
    -o "$tmp.gz" "https://download.db-ip.com/free/dbip-country-lite-$month.mmdb.gz"
gunzip -c "$tmp.gz" > "$tmp"
# A MaxMind DB file ends with its metadata, which starts with this marker.
grep -q "MaxMind.com" "$tmp"
chmod 644 "$tmp"
mv "$tmp" "$dir/dbip-country-lite.mmdb"
