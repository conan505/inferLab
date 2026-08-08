#!/bin/sh
set -eu

script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
compose_file="${script_directory}/compose.yaml"

case "${1:-}" in
  '')
    docker compose --file "$compose_file" down --remove-orphans
    printf 'InferLab stopped. Persistent Raft and gateway data were retained; ephemeral Prometheus history was discarded.\n'
    ;;
  --purge-data)
    docker compose --file "$compose_file" down --remove-orphans --volumes
    printf 'InferLab stopped and its interview-demo Docker volumes were removed; ephemeral Prometheus history was discarded.\n'
    ;;
  *)
    printf 'Usage: %s [--purge-data]\n' "$0" >&2
    exit 2
    ;;
esac
