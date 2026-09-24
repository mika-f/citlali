#!/usr/bin/env bash
# Load-tests a running Citlali with oha (https://github.com/hatoo/oha).
#
#   cargo run --release &
#   bench/bench.sh photo.jpg                 # one image, default queries
#   bench/bench.sh bench/images              # every image in a directory
#   bench/bench.sh photo.jpg 'width=640'     # custom queries
#
# Env: URL (http://127.0.0.1:8080), DURATION (10s), CONNECTIONS (16)
set -euo pipefail

target=${1:?usage: bench/bench.sh <image|dir> [query ...]}
shift
url=${URL:-http://127.0.0.1:8080}
duration=${DURATION:-10s}
connections=${CONNECTIONS:-16}
if [[ -d $target ]]; then
  images=("$target"/*.*)
else
  images=("$target")
fi

if (($# == 0)); then
  set -- \
    "width=400&height=400&fit=cover&format=webp" \
    "width=1280&format=webp" \
    "width=1280&format=avif" \
    "width=1280&format=jpeg" \
    "width=1280" \
    "format=webp" \
    "width=800&height=800&fit=blur&format=webp"
fi

printf '%-10s %-45s %7s %7s %7s %8s %5s\n' image query req/s p50_ms p99_ms out_kb ok%
for image in "${images[@]}"; do
  name=$(basename "$image")
  for query in "$@"; do
    out_kb=$(curl -fsS -X POST --data-binary "@$image" -o /dev/null -w '%{size_download}' \
      "$url/transform?$query" | awk '{printf "%.1f", $1/1024}')
    oha --no-tui --output-format json --wait-ongoing-requests-after-deadline -z "$duration" -c "$connections" \
      -m POST -D "$image" "$url/transform?$query" |
      jq -r --arg i "${name:0:10}" --arg q "$query" --arg kb "$out_kb" '[$i, $q,
          (.summary.requestsPerSec * 10 | floor / 10),
          ((.latencyPercentiles.p50 // 0) * 1000 | floor),
          ((.latencyPercentiles.p99 // 0) * 1000 | floor),
          $kb,
          (.summary.successRate * 100 | floor)] | @tsv' |
      awk -F'\t' '{printf "%-10s %-45s %7s %7s %7s %8s %5s\n", $1, $2, $3, $4, $5, $6, $7}'
  done
done
