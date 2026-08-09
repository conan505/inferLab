#!/bin/sh
set -eu

script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
compose_file="${script_directory}/compose.yaml"
purge_data=0

usage() {
  printf 'Usage: %s [--hosted-edge] [--purge-data]\n' "$0" >&2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --hosted-edge)
      # Hosted and local modes share this Compose project. The flag keeps the
      # operator's teardown intent explicit without requiring hosted secrets.
      ;;
    --purge-data)
      purge_data=1
      ;;
    *)
      usage
      exit 2
      ;;
  esac
  shift
done

if [ "$purge_data" -eq 1 ]; then
  docker compose --file "$compose_file" down --remove-orphans --volumes
  printf 'InferLab stopped and its interview-demo Docker volumes were removed; ephemeral Prometheus history was discarded.\n'
else
  docker compose --file "$compose_file" down --remove-orphans
  printf 'InferLab stopped. Persistent Raft and gateway data were retained; ephemeral Prometheus history was discarded.\n'
fi
