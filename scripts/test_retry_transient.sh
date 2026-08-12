#!/usr/bin/env bash
set -euo pipefail

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
counter="$root/counter"
args_file="$root/args"

cat > "$root/flaky" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
count=0
[ ! -f "$COUNTER" ] || count=$(cat "$COUNTER")
count=$((count + 1))
printf '%s\n' "$count" > "$COUNTER"
printf '%s\n' "$@" > "$ARGS_FILE"
if [ "$MODE" = deterministic ]; then
  echo "compiler rejected the source" >&2
  exit 17
fi
if [ "$MODE" = persistent ] || [ "$count" -lt 2 ]; then
  echo "HTTPError: Response code 502 (Bad Gateway)" >&2
  exit 42
fi
EOF
chmod +x "$root/flaky"

COUNTER="$counter" ARGS_FILE="$args_file" MODE=eventual RETRY_TRANSIENT_DELAYS="0 0" \
  bash scripts/retry-transient.sh "$root/flaky" "argument with spaces" tail
test "$(cat "$counter")" = 2
printf 'argument with spaces\ntail\n' > "$root/expected-args"
cmp "$root/expected-args" "$args_file"

rm -f "$counter"
set +e
COUNTER="$counter" ARGS_FILE="$args_file" MODE=persistent RETRY_TRANSIENT_DELAYS="0 0" \
  bash scripts/retry-transient.sh "$root/flaky"
status=$?
set -e
test "$status" = 42
test "$(cat "$counter")" = 3

rm -f "$counter"
set +e
COUNTER="$counter" ARGS_FILE="$args_file" MODE=deterministic RETRY_TRANSIENT_DELAYS="0 0" \
  bash scripts/retry-transient.sh "$root/flaky"
status=$?
set -e
test "$status" = 17
test "$(cat "$counter")" = 1

echo "transient release retry tests passed"
