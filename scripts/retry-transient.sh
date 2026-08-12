#!/usr/bin/env bash

# Retry only release-packaging failures that look like transient transport
# faults. Deterministic build, signing, and test failures remain immediate.
set -uo pipefail

delay_spec=${RETRY_TRANSIENT_DELAYS:-10 30}
read -r -a delays <<< "$delay_spec"
for delay in "${delays[@]}"; do
  case "$delay" in
    ''|*[!0-9]*) echo "RETRY_TRANSIENT_DELAYS must contain integers" >&2; exit 2 ;;
  esac
  if [ "$delay" -gt 300 ]; then
    echo "RETRY_TRANSIENT_DELAYS entries must not exceed 300 seconds" >&2
    exit 2
  fi
done
if [ "${#delays[@]}" -gt 9 ]; then
  echo "RETRY_TRANSIENT_DELAYS permits at most nine retries" >&2
  exit 2
fi
if [ "$#" -eq 0 ]; then
  echo "usage: retry-transient.sh command [args ...]" >&2
  exit 2
fi

log=$(mktemp "${TMPDIR:-/tmp}/goose-retry.XXXXXX")
trap 'rm -f "$log"' EXIT
command_args=("$@")
attempt=1
attempts=$((${#delays[@]} + 1))

while :; do
  : > "$log"
  echo "release command attempt $attempt/$attempts: ${command_args[0]}"
  "${command_args[@]}" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  if [ "$status" -eq 0 ]; then
    exit 0
  fi

  if ! grep -Eiq \
    '(^|[^[:alnum:]_])(EOF|socket hang up|ECONNRESET|ECONNABORTED|ETIMEDOUT|EAI_AGAIN)([^[:alnum:]_]|$)|HTTP[^[:cntrl:]]*(408|429|5[0-9][0-9])|Response code (408|429|5[0-9][0-9])' \
    "$log"; then
    echo "release command failed with a non-transient error (exit $status): ${command_args[0]}" >&2
    exit "$status"
  fi
  if [ "$attempt" -ge "$attempts" ]; then
    echo "transient release command failed after $attempts attempts (exit $status): ${command_args[0]}" >&2
    exit "$status"
  fi

  wait_seconds=${delays[$((attempt - 1))]}
  echo "transient transport failure; retrying in ${wait_seconds}s" >&2
  sleep "$wait_seconds"
  attempt=$((attempt + 1))
done
