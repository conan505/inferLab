#!/bin/sh
set -eu

script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
compose_file="${script_directory}/compose.yaml"
hosted_compose_file="${script_directory}/compose.hosted-edge.yaml"
demo_port="${INFERLAB_DEMO_PORT:-8080}"
prometheus_port="${INFERLAB_PROMETHEUS_PORT:-9090}"

usage() {
  printf 'Usage: %s [--hosted-edge]\n' "$0" >&2
}

edge_mode="${INFERLAB_PUBLIC_EDGE_MODE:-local}"
case "${1:-}" in
  '')
    ;;
  --hosted-edge)
    edge_mode=hosted
    shift
    ;;
  *)
    usage
    exit 2
    ;;
esac
if [ "$#" -ne 0 ]; then
  usage
  exit 2
fi
case "$edge_mode" in
  local|hosted)
    ;;
  *)
    printf 'INFERLAB_PUBLIC_EDGE_MODE must be local or hosted.\n' >&2
    exit 2
    ;;
esac

compose() {
  if [ "$edge_mode" = hosted ]; then
    docker compose --file "$compose_file" --file "$hosted_compose_file" "$@"
  else
    docker compose --file "$compose_file" "$@"
  fi
}

require_hosted_value() {
  name="$1"
  value="$2"
  if [ -z "$value" ]; then
    printf '%s is required in hosted-edge mode.\n' "$name" >&2
    exit 2
  fi
  case "$value" in
    replace-*|*replace-with*|*replace-route*)
      printf '%s still contains a hosted-edge template placeholder.\n' "$name" >&2
      exit 2
      ;;
  esac
}

is_checked_in_fixture_material() {
  candidate="$1"
  case "$candidate" in
    "$local_public_api_key"|"$local_operator_api_key")
      return 0
      ;;
  esac
  normalized_candidate="$candidate"
  while [ "${normalized_candidate%=}" != "$normalized_candidate" ]; do
    normalized_candidate="${normalized_candidate%=}"
  done
  [ "$normalized_candidate" = "${local_route_private_key%=}" ] \
    || [ "$normalized_candidate" = "${local_route_public_key%=}" ]
}

validate_hosted_public_api_keys() {
  remaining="$1"
  case "$remaining" in
    ''|*[![:graph:]]*|,*|*,|*,,*)
      printf 'INFERLAB_PUBLIC_API_KEYS must be a comma-separated list without whitespace or empty entries.\n' >&2
      exit 2
      ;;
  esac
  while :; do
    case "$remaining" in
      *,*)
        public_key="${remaining%%,*}"
        remaining="${remaining#*,}"
        more_keys=1
        ;;
      *)
        public_key="$remaining"
        more_keys=0
        ;;
    esac
    require_hosted_value INFERLAB_PUBLIC_API_KEYS_entry "$public_key"
    if is_checked_in_fixture_material "$public_key"; then
      printf 'Hosted-edge mode refuses checked-in fixture material in every public/operator role.\n' >&2
      exit 2
    fi
    if [ "$more_keys" -eq 0 ]; then
      break
    fi
  done
}

validate_hosted_operator_api_key() {
  operator_key="$1"
  case "$operator_key" in
    ''|*[![:graph:]]*|*,*)
      printf 'INFERLAB_OPERATOR_API_KEY must be one non-empty visible credential without commas.\n' >&2
      exit 2
      ;;
  esac
  if is_checked_in_fixture_material "$operator_key"; then
    printf 'Hosted-edge mode refuses checked-in fixture material in every public/operator role.\n' >&2
    exit 2
  fi
}

validate_hosted_control_trusted_keys() {
  remaining="$1"
  case "$remaining" in
    ''|*[![:graph:]]*|,*|*,|*,,*)
      printf 'INFERLAB_CONTROL_TRUSTED_KEYS must be a comma-separated list without whitespace or empty entries.\n' >&2
      exit 2
      ;;
  esac
  while :; do
    case "$remaining" in
      *,*)
        trusted_key="${remaining%%,*}"
        remaining="${remaining#*,}"
        more_trusted_keys=1
        ;;
      *)
        trusted_key="$remaining"
        more_trusted_keys=0
        ;;
    esac
    require_hosted_value INFERLAB_CONTROL_TRUSTED_KEYS_entry "$trusted_key"
    trusted_key_id="${trusted_key%%=*}"
    trusted_public_key="${trusted_key#*=}"
    if [ "$trusted_key_id" = "$trusted_key" ] \
      || [ -z "$trusted_key_id" ] \
      || [ -z "$trusted_public_key" ]; then
      printf 'Each INFERLAB_CONTROL_TRUSTED_KEYS entry must use key-id=base64-public-key.\n' >&2
      exit 2
    fi
    if is_checked_in_fixture_material "$trusted_public_key"; then
      printf 'Hosted-edge mode refuses checked-in fixture material in route-signing trust.\n' >&2
      exit 2
    fi
    if [ "$more_trusted_keys" -eq 0 ]; then
      break
    fi
  done
}

validate_positive_integer() {
  name="$1"
  value="$2"
  case "$value" in
    ''|*[!0-9]*|0)
      printf '%s must be a positive integer.\n' "$name" >&2
      exit 2
      ;;
  esac
}

local_public_api_key='inferlab-interview-local-key'
local_operator_api_key='inferlab-interview-local-operator-key'
local_route_private_key='TM0Imyj/ltqdtsNG7BFOD1uKMZ81q6Yk2oz27U+4pvs='
local_route_public_key='PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw='

if [ "$edge_mode" = hosted ]; then
  require_hosted_value INFERLAB_PUBLIC_API_KEYS "${INFERLAB_PUBLIC_API_KEYS:-}"
  require_hosted_value INFERLAB_OPERATOR_API_KEY "${INFERLAB_OPERATOR_API_KEY:-}"
  require_hosted_value INFERLAB_OPERATOR_BIND "${INFERLAB_OPERATOR_BIND:-}"
  require_hosted_value INFERLAB_CONTROL_SIGNING_KEY_ID "${INFERLAB_CONTROL_SIGNING_KEY_ID:-}"
  require_hosted_value INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64 "${INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64:-}"
  require_hosted_value INFERLAB_CONTROL_TRUSTED_KEYS "${INFERLAB_CONTROL_TRUSTED_KEYS:-}"
  validate_hosted_public_api_keys "$INFERLAB_PUBLIC_API_KEYS"
  validate_hosted_operator_api_key "$INFERLAB_OPERATOR_API_KEY"
  validate_hosted_control_trusted_keys "$INFERLAB_CONTROL_TRUSTED_KEYS"

  if is_checked_in_fixture_material "$INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64"; then
    printf 'Hosted-edge mode refuses checked-in fixture material as the route-signing private key.\n' >&2
    exit 2
  fi

  case ",${INFERLAB_PUBLIC_API_KEYS}," in
    *",${INFERLAB_OPERATOR_API_KEY},"*)
      printf 'The public and operator API credentials must be distinct.\n' >&2
      exit 2
      ;;
  esac

  INFERLAB_PUBLIC_MAX_MESSAGES="${INFERLAB_PUBLIC_MAX_MESSAGES:-32}"
  INFERLAB_PUBLIC_MAX_PROMPT_BYTES="${INFERLAB_PUBLIC_MAX_PROMPT_BYTES:-16384}"
  INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS="${INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS:-256}"
  INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE="${INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE:-60}"
  INFERLAB_PUBLIC_RATE_BURST="${INFERLAB_PUBLIC_RATE_BURST:-4}"

  validate_positive_integer INFERLAB_PUBLIC_MAX_MESSAGES "$INFERLAB_PUBLIC_MAX_MESSAGES"
  validate_positive_integer INFERLAB_PUBLIC_MAX_PROMPT_BYTES "$INFERLAB_PUBLIC_MAX_PROMPT_BYTES"
  validate_positive_integer INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS "$INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS"
  validate_positive_integer INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE "$INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE"
  validate_positive_integer INFERLAB_PUBLIC_RATE_BURST "$INFERLAB_PUBLIC_RATE_BURST"

  operator_port="${INFERLAB_OPERATOR_BIND##*:}"
  validate_positive_integer INFERLAB_OPERATOR_BIND_port "$operator_port"

  export INFERLAB_OPERATOR_API_KEY
  export INFERLAB_OPERATOR_BIND
  export INFERLAB_CONTROL_SIGNING_KEY_ID
  export INFERLAB_CONTROL_SIGNING_PRIVATE_KEY_B64
  export INFERLAB_CONTROL_TRUSTED_KEYS
  export INFERLAB_PUBLIC_MAX_MESSAGES
  export INFERLAB_PUBLIC_MAX_PROMPT_BYTES
  export INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS
  export INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE
  export INFERLAB_PUBLIC_RATE_BURST
else
  INFERLAB_PUBLIC_API_KEYS="${INFERLAB_PUBLIC_API_KEYS:-$local_public_api_key}"
fi

case "$INFERLAB_PUBLIC_API_KEYS" in
  *[![:graph:]]*|,*|*,|*,,*)
    printf 'INFERLAB_PUBLIC_API_KEYS must be a comma-separated list without whitespace or empty edge entries.\n' >&2
    exit 2
    ;;
esac
validate_positive_integer INFERLAB_DEMO_PORT "$demo_port"
validate_positive_integer INFERLAB_PROMETHEUS_PORT "$prometheus_port"

export INFERLAB_PUBLIC_API_KEYS

configured_api_keys="$INFERLAB_PUBLIC_API_KEYS"
demo_api_key="${configured_api_keys%%,*}"
if [ "$edge_mode" = hosted ]; then
  operator_api_key="$INFERLAB_OPERATOR_API_KEY"
  demo_api_key_message='configured through the environment (value not printed)'
elif [ -n "${INFERLAB_PUBLIC_API_KEYS:-}" ] && [ "$demo_api_key" != "$local_public_api_key" ]; then
  demo_api_key_message='configured through INFERLAB_PUBLIC_API_KEYS (value not printed)'
else
  demo_api_key_message='inferlab-interview-local-key (public local fixture only)'
fi

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

public_internal_route_is_hidden() {
  status_code="$(
    curl --silent --max-time 2 --noproxy '*' --output /dev/null --write-out '%{http_code}' \
      --header "authorization: Bearer ${demo_api_key}" \
      "http://127.0.0.1:${demo_port}/internal/workers" || true
  )"
  [ "$status_code" = 404 ]
}

local_internal_route_is_available() {
  curl --fail --silent --max-time 2 --noproxy '*' \
    --header "authorization: Bearer ${demo_api_key}" \
    "http://127.0.0.1:${demo_port}/internal/workers" >/dev/null 2>&1
}

operator_rejects_public_key() {
  status_code="$(
    compose exec -T gateway \
      curl --silent --max-time 2 --noproxy '*' --output /dev/null --write-out '%{http_code}' \
        --header "authorization: Bearer ${demo_api_key}" \
        "http://127.0.0.1:${operator_port}/internal/workers" || true
  )"
  [ "$status_code" = 401 ]
}

operator_accepts_operator_key() {
  compose exec -T gateway \
    curl --fail --silent --max-time 2 --noproxy '*' \
      --header "authorization: Bearer ${operator_api_key}" \
      "http://127.0.0.1:${operator_port}/internal/workers" >/dev/null 2>&1
}

edge_endpoints_are_ready() {
  if [ "$edge_mode" = hosted ]; then
    public_internal_route_is_hidden \
      && operator_rejects_public_key \
      && operator_accepts_operator_key
  else
    local_internal_route_is_available
  fi
}

if ! docker info >/dev/null 2>&1; then
  printf 'Docker is installed, but its engine is not available.\n' >&2
  exit 1
fi

compose config --quiet
if [ "${INFERLAB_SKIP_BUILD:-0}" = 1 ]; then
  compose up --detach
else
  compose up --build --detach
fi

attempt=1
maximum_attempts=120
while [ "$attempt" -le "$maximum_attempts" ]; do
  if curl --fail --silent --max-time 2 --noproxy '*' "http://127.0.0.1:${demo_port}/readyz" >/dev/null \
    && curl --fail --silent --max-time 2 --noproxy '*' "http://127.0.0.1:${prometheus_port}/-/ready" >/dev/null \
    && prometheus_all_targets_up \
    && edge_endpoints_are_ready; then
    printf '\nInferLab interview topology is ready.\n'
    printf 'Edge mode: %s\n' "$edge_mode"
    printf 'Showcase: http://127.0.0.1:%s/\n' "$demo_port"
    printf 'Demo key: %s\n' "$demo_api_key_message"
    if [ "$edge_mode" = hosted ]; then
      printf 'Operator listener: private Compose network only (not host-published)\n'
    else
      printf 'Operator diagnostics: authenticated on the existing loopback public listener\n'
    fi
    printf 'Prometheus: http://127.0.0.1:%s/\n' "$prometheus_port"
    printf 'Scrape targets: http://127.0.0.1:%s/targets\n' "$prometheus_port"
    if [ "$edge_mode" = hosted ]; then
      printf 'Public limits: messages=%s prompt_bytes=%s output_tokens=%s rate_per_minute=%s burst=%s\n' \
        "$INFERLAB_PUBLIC_MAX_MESSAGES" \
        "$INFERLAB_PUBLIC_MAX_PROMPT_BYTES" \
        "$INFERLAB_PUBLIC_MAX_OUTPUT_TOKENS" \
        "$INFERLAB_PUBLIC_RATE_REQUESTS_PER_MINUTE" \
        "$INFERLAB_PUBLIC_RATE_BURST"
    fi
    printf 'Bounded PromQL examples:\n'
    printf '  sum by (service) (rate(inferlab_http_requests_total[1m]))\n'
    printf '  sum by (service, status_class) (rate(inferlab_http_requests_total[1m]))\n'
    printf '  histogram_quantile(0.95, sum by (le, service) (rate(inferlab_http_handler_duration_seconds_bucket[5m])))\n'
    if [ "$edge_mode" = hosted ]; then
      printf 'Topology: three private controls, two private CPU workers, one loopback public listener, one private operator listener, one loopback Prometheus UI\n\n'
      printf 'Live routing state through the private operator listener:\n'
      compose exec -T gateway \
        curl --silent --show-error --noproxy '*' \
          --header "authorization: Bearer ${operator_api_key}" \
          "http://127.0.0.1:${operator_port}/internal/workers"
    else
      printf 'Topology: three private controls, two private CPU workers, one loopback gateway listener, one loopback Prometheus UI\n\n'
      printf 'Live routing state through the local authenticated listener:\n'
      curl --silent --show-error --noproxy '*' \
        --header "authorization: Bearer ${demo_api_key}" \
        "http://127.0.0.1:${demo_port}/internal/workers"
    fi
    printf '\n'
    if [ "$edge_mode" = hosted ]; then
      printf 'Hosted-edge rehearsal is local only: add provider-managed HTTPS, network controls, and DDoS/WAF protection before any internet exposure.\n'
    fi
    exit 0
  fi

  attempt=$((attempt + 1))
  sleep 1
done

printf 'InferLab did not become ready within %s seconds.\n' "$maximum_attempts" >&2
compose ps >&2
compose logs --tail 100 configure gateway prometheus >&2
exit 1
