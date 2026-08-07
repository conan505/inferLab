#!/bin/sh
set -eu

control_urls="${INFERLAB_DEMO_CONTROL_URLS:-http://control-a:7000 http://control-b:7000 http://control-c:7000}"
routing_policy="${INFERLAB_DEMO_ROUTING_POLICY:-least-in-flight}"
if [ -n "${INFERLAB_DEMO_WORKERS_JSON:-}" ]; then
  workers_json="$INFERLAB_DEMO_WORKERS_JSON"
else
  workers_json='[{"id":"cpu-worker-a","base_url":"http://worker-a:9101","weight":1},{"id":"cpu-worker-b","base_url":"http://worker-b:9101","weight":1}]'
fi
payload="{\"routing_policy\":\"${routing_policy}\",\"workers\":${workers_json}}"
response_file="/tmp/inferlab-control-response.json"
attempt=1
maximum_attempts=120

while [ "$attempt" -le "$maximum_attempts" ]; do
  for control_url in $control_urls; do
    status_code="$(
      curl \
        --silent \
        --show-error \
        --connect-timeout 1 \
        --max-time 4 \
        --output "$response_file" \
        --write-out '%{http_code}' \
        --request PUT \
        --header 'content-type: application/json' \
        --data "$payload" \
        "${control_url}/v1/control/config" || true
    )"
    if [ "$status_code" = "200" ]; then
      printf 'InferLab routing configuration committed through %s\n' "$control_url"
      cat "$response_file"
      printf '\n'
      exit 0
    fi
  done

  attempt=$((attempt + 1))
  sleep 0.25
done

printf 'Timed out waiting for a writable InferLab Raft leader. Last response:\n' >&2
cat "$response_file" >&2 || true
printf '\n' >&2
exit 1
