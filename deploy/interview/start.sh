#!/bin/sh
set -eu

script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
compose_file="${script_directory}/compose.yaml"
demo_port="${INFERLAB_DEMO_PORT:-8080}"
prometheus_port="${INFERLAB_PROMETHEUS_PORT:-9090}"
configured_api_keys="${INFERLAB_PUBLIC_API_KEYS:-inferlab-interview-local-key}"
demo_api_key="${configured_api_keys%%,*}"
if [ -n "${INFERLAB_PUBLIC_API_KEYS:-}" ]; then
  demo_api_key_message="configured through INFERLAB_PUBLIC_API_KEYS (value not printed)"
else
  demo_api_key_message="inferlab-interview-local-key (local default only)"
fi

case "$demo_port" in
  ''|*[!0-9]*)
    printf 'INFERLAB_DEMO_PORT must be a numeric TCP port.\n' >&2
    exit 2
    ;;
esac

case "$prometheus_port" in
  ''|*[!0-9]*)
    printf 'INFERLAB_PROMETHEUS_PORT must be a numeric TCP port.\n' >&2
    exit 2
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  printf 'Docker is required but was not found in PATH.\n' >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  printf 'curl is required but was not found in PATH.\n' >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  printf 'Python 3 is required but was not found in PATH.\n' >&2
  exit 1
fi

prometheus_query_equals() {
  query="$1"
  expected="$2"
  curl --fail --silent --max-time 2 --noproxy '*' --get \
    --data-urlencode "query=${query}" \
    "http://127.0.0.1:${prometheus_port}/api/v1/query" \
    | python3 -c '
import json
import sys

payload = json.load(sys.stdin)
result = payload.get("data", {}).get("result", [])
valid = (
    payload.get("status") == "success"
    and len(result) == 1
    and len(result[0].get("value", [])) == 2
    and float(result[0]["value"][1]) == float(sys.argv[1])
)
raise SystemExit(0 if valid else 1)
' "$expected" 2>/dev/null
}

prometheus_all_targets_up() {
  prometheus_query_equals 'count(up{job="inferlab-interview"})' 6 \
    && prometheus_query_equals 'count(up{job="inferlab-interview"} == 1)' 6
}

if ! docker info >/dev/null 2>&1; then
  printf 'Docker is installed, but its engine is not available.\n' >&2
  exit 1
fi

docker compose --file "$compose_file" config --quiet
if [ "${INFERLAB_SKIP_BUILD:-0}" = "1" ]; then
  docker compose --file "$compose_file" up --detach
else
  docker compose --file "$compose_file" up --build --detach
fi

attempt=1
maximum_attempts=120
while [ "$attempt" -le "$maximum_attempts" ]; do
  if curl --fail --silent --max-time 2 --noproxy '*' "http://127.0.0.1:${demo_port}/readyz" >/dev/null \
    && curl --fail --silent --max-time 2 --noproxy '*' "http://127.0.0.1:${prometheus_port}/-/ready" >/dev/null \
    && prometheus_all_targets_up; then
    printf '\nInferLab interview topology is ready.\n'
    printf 'Showcase: http://127.0.0.1:%s/\n' "$demo_port"
    printf 'Demo key: %s\n' "$demo_api_key_message"
    printf 'Prometheus: http://127.0.0.1:%s/\n' "$prometheus_port"
    printf 'Scrape targets: http://127.0.0.1:%s/targets\n' "$prometheus_port"
    printf 'Bounded PromQL examples:\n'
    printf '  sum by (service) (rate(inferlab_http_requests_total[1m]))\n'
    printf '  sum by (service, status_class) (rate(inferlab_http_requests_total[1m]))\n'
    printf '  histogram_quantile(0.95, sum by (le, service) (rate(inferlab_http_handler_duration_seconds_bucket[5m])))\n'
    printf 'Topology: three private controls, two private CPU workers, one loopback gateway, one loopback Prometheus UI\n\n'
    printf 'Live routing state:\n'
    curl \
      --silent \
      --show-error \
      --noproxy '*' \
      --header "authorization: Bearer ${demo_api_key}" \
      "http://127.0.0.1:${demo_port}/internal/workers"
    printf '\n'
    exit 0
  fi

  attempt=$((attempt + 1))
  sleep 1
done

printf 'InferLab did not become ready within %s seconds.\n' "$maximum_attempts" >&2
docker compose --file "$compose_file" ps >&2
docker compose --file "$compose_file" logs --tail 100 configure gateway prometheus >&2
exit 1
