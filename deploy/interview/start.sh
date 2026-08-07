#!/bin/sh
set -eu

script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
compose_file="${script_directory}/compose.yaml"
demo_port="${INFERLAB_DEMO_PORT:-8080}"
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

if ! command -v docker >/dev/null 2>&1; then
  printf 'Docker is required but was not found in PATH.\n' >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  printf 'curl is required but was not found in PATH.\n' >&2
  exit 1
fi

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
  if curl --fail --silent --max-time 2 "http://127.0.0.1:${demo_port}/readyz" >/dev/null; then
    printf '\nInferLab interview topology is ready.\n'
    printf 'Showcase: http://127.0.0.1:%s/\n' "$demo_port"
    printf 'Demo key: %s\n' "$demo_api_key_message"
    printf 'Topology: three private controls, two private CPU workers, one loopback gateway\n\n'
    printf 'Live routing state:\n'
    curl \
      --silent \
      --show-error \
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
docker compose --file "$compose_file" logs --tail 100 configure gateway >&2
exit 1
