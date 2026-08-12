#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/goose-installer-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REAL_MV=$(command -v mv)

mkdir -p "$TEST_ROOT/mock-bin"
cat > "$TEST_ROOT/mock-bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
retries=0
retry_delay=""
retry_max_time=""
connect_timeout=""
max_time=""
max_filesize=""
retry_all_errors=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    --retry) retries=$2; shift 2 ;;
    --retry-delay) retry_delay=$2; shift 2 ;;
    --retry-max-time) retry_max_time=$2; shift 2 ;;
    --connect-timeout) connect_timeout=$2; shift 2 ;;
    --max-time) max_time=$2; shift 2 ;;
    --max-filesize) max_filesize=$2; shift 2 ;;
    --retry-all-errors) retry_all_errors=1; shift ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
[ "$retries" = 4 ]
[ "$retry_delay" = 2 ]
[ "$retry_max_time" = 120 ]
[ "$connect_timeout" = 15 ]
[ "$retry_all_errors" = 0 ]
if [[ "$url" == https://api.github.com/*/releases/latest ]]; then
  [ -z "$output" ]
  [ "$max_time" = 120 ]
  [ -z "$max_filesize" ]
  printf '{"tag_name":"v1.46.1"}\n'
  exit 0
fi
[ -n "$output" ]
if [[ "$url" == *.sha256 ]]; then
  [ "$max_time" = 120 ]
  [ "$max_filesize" = 1024 ]
else
  [ "$max_time" = 600 ]
  [ "$max_filesize" = 1073741824 ]
fi
if [ "${CURL_FAIL_STABLE:-0}" = 1 ] && [[ "$url" == */stable/* ]] && \
  [[ "$url" != *.sha256 ]]; then
  exit 22
fi
if [[ "$url" == *.sha256 ]]; then
  printf '%s  %s\n' "$(sha256sum "$FIXTURE_ARCHIVE" | awk '{print $1}')" \
    "$(basename "${url%.sha256}")" > "$output"
else
  cp "$FIXTURE_ARCHIVE" "$output"
fi
MOCK_CURL
chmod +x "$TEST_ROOT/mock-bin/curl"

make_fixture() {
  local name=$1 payload=$2
  local dir="$TEST_ROOT/$name"
  mkdir -p "$dir/package"
  printf '%s' "$payload" > "$dir/package/goose"
  chmod +x "$dir/package/goose"
  tar -cjf "$dir/archive.tar.bz2" -C "$dir/package" goose
  printf '%s\n' "$dir/archive.tar.bz2"
}

run_installer() {
  local fixture=$1 bin_dir=$2 work_dir=$3 tmp_dir=$4
  mkdir -p "$bin_dir" "$work_dir" "$tmp_dir"
  (
    cd "$work_dir"
    PATH="$bin_dir:$TEST_ROOT/mock-bin:$PATH" \
      SHELL=/bin/bash CONFIGURE=false INSTALL_OS=linux GOOSE_LINUX_VARIANT=standard \
      GOOSE_BIN_DIR="$bin_dir" TMPDIR="$tmp_dir" FIXTURE_ARCHIVE="$fixture" \
      bash "$REPO_ROOT/download_cli.sh" >/dev/null
  )
}

# Every release transfer carries the bounded retry policy asserted by the mock.
# If stable remains unavailable after curl exhausts those retries, the installer
# resolves GitHub's latest immutable release and applies a distinct metadata
# transfer limit before fetching the same archive contract there.
fixture=$(make_fixture retry-policy retry-success)
mkdir -p "$TEST_ROOT/retry-policy/bin" "$TEST_ROOT/retry-policy/work" \
  "$TEST_ROOT/retry-policy/tmp"
CURL_FAIL_STABLE=1 run_installer "$fixture" "$TEST_ROOT/retry-policy/bin" \
  "$TEST_ROOT/retry-policy/work" "$TEST_ROOT/retry-policy/tmp"
test "$(cat "$TEST_ROOT/retry-policy/bin/goose")" = retry-success

run_windows_installer() {
  local fixture=$1 bin_dir=$2 work_dir=$3 tmp_dir=$4 mock_dir=${5:-$TEST_ROOT/mock-bin}
  mkdir -p "$bin_dir" "$work_dir" "$tmp_dir"
  (
    cd "$work_dir"
    PATH="$bin_dir:$mock_dir:$PATH" \
      SHELL=/bin/bash CONFIGURE=false INSTALL_OS=windows GOOSE_WINDOWS_VARIANT=standard \
      GOOSE_BIN_DIR="$bin_dir" TMPDIR="$tmp_dir" FIXTURE_ARCHIVE="$fixture" \
      bash "$REPO_ROOT/download_cli.sh" >/dev/null
  )
}

# Fixed names in the caller's directory and an installed-binary symlink must
# never be followed or overwritten.
fixture=$(make_fixture symlink safe-new)
mkdir -p "$TEST_ROOT/symlink/bin" "$TEST_ROOT/symlink/work" "$TEST_ROOT/symlink/tmp"
printf 'victim' > "$TEST_ROOT/symlink/victim"
ln -s "$TEST_ROOT/symlink/victim" "$TEST_ROOT/symlink/work/goose-x86_64-unknown-linux-gnu.tar.bz2"
ln -s "$TEST_ROOT/symlink/victim" "$TEST_ROOT/symlink/work/goose-x86_64-unknown-linux-gnu.tar.bz2.sha256"
ln -s "$TEST_ROOT/symlink/victim" "$TEST_ROOT/symlink/work/tar_error.log"
ln -s "$TEST_ROOT/symlink/victim" "$TEST_ROOT/symlink/bin/goose"
run_installer "$fixture" "$TEST_ROOT/symlink/bin" "$TEST_ROOT/symlink/work" "$TEST_ROOT/symlink/tmp"
test "$(cat "$TEST_ROOT/symlink/victim")" = victim
test "$(cat "$TEST_ROOT/symlink/bin/goose")" = safe-new
test ! -L "$TEST_ROOT/symlink/bin/goose"
test -L "$TEST_ROOT/symlink/work/tar_error.log"
test -z "$(find "$TEST_ROOT/symlink/tmp" -mindepth 1 -print -quit)"

# A checksum-valid archive is still rejected if it contains links or any
# member outside the exact CLI package contract.
mkdir -p "$TEST_ROOT/archive-validation/package" "$TEST_ROOT/archive-validation/bin" \
  "$TEST_ROOT/archive-validation/work" "$TEST_ROOT/archive-validation/tmp"
printf 'binary' > "$TEST_ROOT/archive-validation/package/goose"
ln -s goose "$TEST_ROOT/archive-validation/package/alias"
tar -cjf "$TEST_ROOT/archive-validation/bad.tar.bz2" \
  -C "$TEST_ROOT/archive-validation/package" goose alias
set +e
run_installer "$TEST_ROOT/archive-validation/bad.tar.bz2" \
  "$TEST_ROOT/archive-validation/bin" "$TEST_ROOT/archive-validation/work" \
  "$TEST_ROOT/archive-validation/tmp" >/dev/null 2>&1
archive_status=$?
set -e
test "$archive_status" -ne 0
test ! -e "$TEST_ROOT/archive-validation/bin/goose"
test -z "$(find "$TEST_ROOT/archive-validation/tmp" -mindepth 1 -print -quit)"

# A dead process lock is recoverable.
mkdir -p "$TEST_ROOT/stale/bin/.goose-install.lock"
printf '99999999\n' > "$TEST_ROOT/stale/bin/.goose-install.lock/pid"
run_installer "$fixture" "$TEST_ROOT/stale/bin" "$TEST_ROOT/stale/work" "$TEST_ROOT/stale/tmp"
test ! -e "$TEST_ROOT/stale/bin/.goose-install.lock"

# Concurrent installers serialize only the atomic promotion. Every invocation
# succeeds, the result is one whole payload, and no staging/rollback files leak.
mkdir -p "$TEST_ROOT/concurrent/bin" "$TEST_ROOT/concurrent/work" "$TEST_ROOT/concurrent/tmp"
pids=()
for index in 0 1 2 3 4 5 6 7; do
  concurrent_fixture=$(make_fixture "concurrent-$index" "complete-$index")
  run_installer "$concurrent_fixture" "$TEST_ROOT/concurrent/bin" \
    "$TEST_ROOT/concurrent/work" "$TEST_ROOT/concurrent/tmp" &
  pids+=("$!")
done
for pid in "${pids[@]}"; do
  wait "$pid"
done
grep -Eq '^complete-[0-7]$' "$TEST_ROOT/concurrent/bin/goose"
test -z "$(find "$TEST_ROOT/concurrent/bin" -maxdepth 1 \( -name '.goose-*' -o -name '*.old' \) -print -quit)"
test -z "$(find "$TEST_ROOT/concurrent/tmp" -mindepth 1 -print -quit)"

# Windows stages the executable and DLLs as one rollback unit. The executable
# is absent during the narrow multi-file commit window and promoted last; an
# injected second-DLL failure restores the complete prior runtime.
mkdir -p "$TEST_ROOT/windows/goose-package" "$TEST_ROOT/windows/bin" \
  "$TEST_ROOT/windows/work" "$TEST_ROOT/windows/tmp" "$TEST_ROOT/windows/mock-bin"
printf 'new-exe' > "$TEST_ROOT/windows/goose-package/goose.exe"
printf 'new-one' > "$TEST_ROOT/windows/goose-package/one.dll"
printf 'new-two' > "$TEST_ROOT/windows/goose-package/two.dll"
(
  cd "$TEST_ROOT/windows"
  python3 -m zipfile -c archive.zip goose-package/goose.exe \
    goose-package/one.dll goose-package/two.dll
)
cp "$TEST_ROOT/mock-bin/curl" "$TEST_ROOT/windows/mock-bin/curl"
cat > "$TEST_ROOT/windows/mock-bin/mv" <<'MOCK_WINDOWS_MV'
#!/usr/bin/env bash
set -euo pipefail
source_path=${@: -2:1}
destination=${@: -1}
if [[ "$source_path" == */.goose.stage.*/two.dll ]] && [ "$destination" = "$FAIL_DLL_TARGET" ]; then
  exit 1
fi
exec "$REAL_MV" "$@"
MOCK_WINDOWS_MV
chmod +x "$TEST_ROOT/windows/mock-bin/mv"
printf 'old-exe' > "$TEST_ROOT/windows/bin/goose.exe"
printf 'old-one' > "$TEST_ROOT/windows/bin/one.dll"
printf 'old-two' > "$TEST_ROOT/windows/bin/two.dll"
set +e
FAIL_DLL_TARGET="$TEST_ROOT/windows/bin/two.dll" REAL_MV="$REAL_MV" \
  run_windows_installer "$TEST_ROOT/windows/archive.zip" "$TEST_ROOT/windows/bin" \
  "$TEST_ROOT/windows/work" "$TEST_ROOT/windows/tmp" "$TEST_ROOT/windows/mock-bin" \
  >/dev/null 2>&1
windows_failure_status=$?
set -e
test "$windows_failure_status" -ne 0
test "$(cat "$TEST_ROOT/windows/bin/goose.exe")" = old-exe
test "$(cat "$TEST_ROOT/windows/bin/one.dll")" = old-one
test "$(cat "$TEST_ROOT/windows/bin/two.dll")" = old-two
test -z "$(find "$TEST_ROOT/windows/bin" -maxdepth 1 -name '.goose.*' -print -quit)"

run_windows_installer "$TEST_ROOT/windows/archive.zip" "$TEST_ROOT/windows/bin" \
  "$TEST_ROOT/windows/work" "$TEST_ROOT/windows/tmp"
test "$(cat "$TEST_ROOT/windows/bin/goose.exe")" = new-exe
test "$(cat "$TEST_ROOT/windows/bin/one.dll")" = new-one
test "$(cat "$TEST_ROOT/windows/bin/two.dll")" = new-two
test -z "$(find "$TEST_ROOT/windows/bin" -maxdepth 1 -name '.goose.*' -print -quit)"

# A failed atomic promotion preserves the prior executable and removes its
# candidate and lock.
mkdir -p "$TEST_ROOT/failure/mock-bin" "$TEST_ROOT/failure/bin" "$TEST_ROOT/failure/tmp"
cp "$TEST_ROOT/mock-bin/curl" "$TEST_ROOT/failure/mock-bin/curl"
cat > "$TEST_ROOT/failure/mock-bin/mv" <<'MOCK_MV'
#!/usr/bin/env bash
set -euo pipefail
source_path=${@: -2:1}
destination=${@: -1}
if [[ "$(basename "$source_path")" == .goose.install.* ]] && [ "$destination" = "$FAIL_TARGET" ]; then
  exit 1
fi
exec "$REAL_MV" "$@"
MOCK_MV
chmod +x "$TEST_ROOT/failure/mock-bin/mv"
printf 'old-version' > "$TEST_ROOT/failure/bin/goose"
set +e
(
  cd "$TEST_ROOT/failure"
  PATH="$TEST_ROOT/failure/bin:$TEST_ROOT/failure/mock-bin:$PATH" \
    SHELL=/bin/bash CONFIGURE=false INSTALL_OS=linux GOOSE_LINUX_VARIANT=standard \
    GOOSE_BIN_DIR="$TEST_ROOT/failure/bin" TMPDIR="$TEST_ROOT/failure/tmp" \
    FIXTURE_ARCHIVE="$fixture" FAIL_TARGET="$TEST_ROOT/failure/bin/goose" REAL_MV="$REAL_MV" \
    bash "$REPO_ROOT/download_cli.sh" >/dev/null 2>&1
)
failure_status=$?
set -e
test "$failure_status" -ne 0
test "$(cat "$TEST_ROOT/failure/bin/goose")" = old-version
test -z "$(find "$TEST_ROOT/failure/bin" -maxdepth 1 -name '.goose-*' -print -quit)"

echo "download_cli security tests passed"
