#!/usr/bin/env bash
set -eu

TMP_DIR=""
INSTALL_CANDIDATE=""
LOCK_DIR=""
LOCK_HELD=false
WINDOWS_STAGE_DIR=""
WINDOWS_ROLLBACK_DIR=""
WINDOWS_MANIFEST=""
WINDOWS_PROMOTED_MANIFEST=""
WINDOWS_COMMITTED=false

release_install_lock() {
  if [ "$LOCK_HELD" = true ]; then
    rm -f -- "$LOCK_DIR/pid" 2>/dev/null || true
    rmdir -- "$LOCK_DIR" 2>/dev/null || true
    LOCK_HELD=false
  fi
}

rollback_windows_install() {
  local name target failed=false
  if [ -z "$WINDOWS_ROLLBACK_DIR" ] || [ -z "$WINDOWS_MANIFEST" ] || \
     [ ! -f "$WINDOWS_MANIFEST" ]; then
    return 0
  fi
  if [ -n "$WINDOWS_PROMOTED_MANIFEST" ] && [ -f "$WINDOWS_PROMOTED_MANIFEST" ]; then
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      rm -f -- "$GOOSE_BIN_DIR/$name" 2>/dev/null || failed=true
    done < "$WINDOWS_PROMOTED_MANIFEST"
  fi
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    target="$GOOSE_BIN_DIR/$name"
    if [ -e "$WINDOWS_ROLLBACK_DIR/$name" ] || [ -L "$WINDOWS_ROLLBACK_DIR/$name" ]; then
      mv -- "$WINDOWS_ROLLBACK_DIR/$name" "$target" 2>/dev/null || failed=true
    fi
  done < "$WINDOWS_MANIFEST"
  if [ "$failed" = true ]; then
    echo "Error: automatic Windows rollback was incomplete; recovery files remain in $WINDOWS_ROLLBACK_DIR" >&2
    return 1
  fi
  rm -rf -- "$WINDOWS_ROLLBACK_DIR" 2>/dev/null || true
  WINDOWS_ROLLBACK_DIR=""
  WINDOWS_MANIFEST=""
  WINDOWS_PROMOTED_MANIFEST=""
  return 0
}

cleanup() {
  if [ -n "$INSTALL_CANDIDATE" ]; then
    rm -f -- "$INSTALL_CANDIDATE" 2>/dev/null || true
  fi
  if [ "$WINDOWS_COMMITTED" != true ]; then
    rollback_windows_install || true
  fi
  if [ -n "$WINDOWS_STAGE_DIR" ]; then
    rm -rf -- "$WINDOWS_STAGE_DIR" 2>/dev/null || true
  fi
  if [ "$WINDOWS_COMMITTED" = true ] && [ -n "$WINDOWS_ROLLBACK_DIR" ]; then
    rm -rf -- "$WINDOWS_ROLLBACK_DIR" 2>/dev/null || true
  fi
  release_install_lock
  if [ -n "$TMP_DIR" ]; then
    rm -rf -- "$TMP_DIR" 2>/dev/null || true
  fi
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

##############################################################################
# goose CLI Install Script
#
# This script downloads the latest stable 'goose' CLI binary from GitHub releases
# and installs it to your system.
#
# Supported OS: macOS (darwin), Linux, Windows (MSYS2/Git Bash/WSL), Android (Termux)
# Supported Architectures: x86_64, arm64
#
# Usage:
#   curl -fsSL https://github.com/ViaTechSystems/goose/releases/download/stable/download_cli.sh | bash
#
# Environment variables:
#   GOOSE_BIN_DIR  - Directory to which goose will be installed (default: $HOME/.local/bin)
#   GOOSE_VERSION  - Optional: specific version to install (e.g., "v1.0.25"). Overrides CANARY. Can be in the format vX.Y.Z, vX.Y.Z-suffix, or X.Y.Z
#   GOOSE_PROVIDER - Optional: provider for goose
#   GOOSE_MODEL    - Optional: model for goose
#   GOOSE_LINUX_VARIANT - Optional: Linux package variant to install (`standard`, `vulkan`, or `musl`)
#   GOOSE_WINDOWS_VARIANT - Optional: Windows package variant to install (`standard` or `cuda`)
#   CANARY         - Optional: if set to "true", downloads from canary release instead of stable
#   CONFIGURE      - Optional: if set to "false", disables running goose configure interactively
#   ** other provider specific environment variables (eg. DATABRICKS_HOST)
##############################################################################

# --- 1) Check for dependencies ---
# Check for curl
if ! command -v curl >/dev/null 2>&1; then
  echo "Error: 'curl' is required to download goose. Please install curl and try again."
  exit 1
fi

# Check for tar or unzip (depending on OS)
if ! command -v tar >/dev/null 2>&1 && ! command -v unzip >/dev/null 2>&1; then
  echo "Error: Either 'tar' or 'unzip' is required to extract goose. Please install one and try again."
  exit 1
fi

# Check for required extraction tools based on detected OS
if [ "${OS:-}" = "windows" ]; then
  # Windows uses PowerShell's built-in Expand-Archive - check if PowerShell is available
  if ! command -v powershell.exe >/dev/null 2>&1 && ! command -v pwsh >/dev/null 2>&1; then
    echo "Warning: PowerShell is recommended to extract Windows packages but was not found."
    echo "Falling back to unzip if available."
  fi
else
  if ! command -v tar >/dev/null 2>&1; then
    echo "Error: 'tar' is required to extract packages for ${OS:-unknown}. Please install tar and try again."
    exit 1
  fi
fi


# --- 2) Variables ---
REPO="ViaTechSystems/goose"
OUT_FILE="goose"

# Set default bin directory based on detected OS environment
if [[ "${WINDIR:-}" ]] || [[ "${windir:-}" ]] || [[ "$OSTYPE" == "msys" ]] || [[ "$OSTYPE" == "cygwin" ]]; then
    # Native Windows environments - use Windows user profile path
    DEFAULT_BIN_DIR="$USERPROFILE/goose"
else
    # Linux, macOS, and WSL all use the same bin directory
    DEFAULT_BIN_DIR="$HOME/.local/bin"
fi

GOOSE_BIN_DIR="${GOOSE_BIN_DIR:-$DEFAULT_BIN_DIR}"
RELEASE="${CANARY:-false}"
CONFIGURE="${CONFIGURE:-true}"
GOOSE_LINUX_VARIANT="${GOOSE_LINUX_VARIANT:-}"
GOOSE_WINDOWS_VARIANT="${GOOSE_WINDOWS_VARIANT:-standard}"
if [ -n "${GOOSE_VERSION:-}" ]; then
  # Validate the version format
  if [[ ! "$GOOSE_VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+(-.*)?$ ]]; then
    echo "[error]: invalid version '$GOOSE_VERSION'."
    echo "  expected: semver format vX.Y.Z, vX.Y.Z-suffix, or X.Y.Z"
    exit 1
  fi
  GOOSE_VERSION=$(echo "$GOOSE_VERSION" | sed 's/^v\{0,1\}/v/') # Ensure the version string is prefixed with 'v' if not already present
  RELEASE_TAG="$GOOSE_VERSION"
else
  # If GOOSE_VERSION is not set, fall back to existing behavior for backwards compatibility
  RELEASE_TAG="$([[ "$RELEASE" == "true" ]] && echo "canary" || echo "stable")"
fi

# --- 3) Detect OS/Architecture ---
# Allow explicit override for automation or when auto-detection is wrong:
#   INSTALL_OS=linux|windows|darwin
if [ -n "${INSTALL_OS:-}" ]; then
  case "${INSTALL_OS}" in
    linux|windows|darwin) OS="${INSTALL_OS}" ;;
    *) echo "[error]: unsupported INSTALL_OS='${INSTALL_OS}' (expected: linux|windows|darwin)"; exit 1 ;;
  esac
else
  # Better OS detection for Windows environments, with safer WSL handling.
  # If explicit Windows-like shells/variables are present (MSYS/Cygwin), treat as windows.
  if [[ "${WINDIR:-}" ]] || [[ "${windir:-}" ]] || [[ "$OSTYPE" == "msys" ]] || [[ "$OSTYPE" == "cygwin" ]]; then
    OS="windows"
  elif [[ -n "${TERMUX_VERSION:-}" ]]; then
    # Termux on Android: treat as Linux before the Windows mount heuristic,
    # since /d may exist on Android and would incorrectly match as Windows.
    OS="linux"
  elif [[ -f "/proc/version" ]] && grep -q "Microsoft\|WSL" /proc/version 2>/dev/null; then
    # WSL is a Linux environment regardless of the current working directory.
    # The PWD (e.g. /mnt/c/) does not change the kernel — always install Linux.
    OS="linux"
  elif [[ "$OSTYPE" == "darwin"* ]]; then
    OS="darwin"
  elif [[ "$PWD" =~ ^/[a-zA-Z]/ ]] && [[ -d "/c" || -d "/d" || -d "/e" ]]; then
    # Check for Windows-style mount points (like in Git Bash)
    OS="windows"
  else
    # Fallback to uname for other systems
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  fi
fi

ARCH=$(uname -m)

# Handle Windows environments (MSYS2, Git Bash, Cygwin, WSL)
case "$OS" in
  linux|darwin|windows) ;;
  mingw*|msys*|cygwin*)
    OS="windows"
    ;;
  *)
    echo "Error: Unsupported OS '$OS'. goose currently supports Linux, macOS, and Windows."
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64)
    ARCH="x86_64"
    ;;
  arm64|aarch64)
    # Some systems use 'arm64' and some 'aarch64' – standardize to 'aarch64'
    ARCH="aarch64"
    ;;
  *)
    echo "Error: Unsupported architecture '$ARCH'."
    exit 1
    ;;
esac

detect_linux_musl() {
  if [[ "$OSTYPE" == "linux-musl"* ]]; then
    return 0
  fi

  if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    return 0
  fi

  return 1
}

# Termux on Android: the musl portable build is the best fit (no system-keyring, no local-inference).
if [ "$OS" = "linux" ] && [ -n "${TERMUX_VERSION:-}" ] && [ -z "$GOOSE_LINUX_VARIANT" ]; then
  echo "Termux detected (v$TERMUX_VERSION). Using musl portable build."
  GOOSE_LINUX_VARIANT="musl"
fi

if [ "$OS" = "linux" ] && [ -z "$GOOSE_LINUX_VARIANT" ]; then
  if detect_linux_musl; then
    GOOSE_LINUX_VARIANT="musl"
  else
    GOOSE_LINUX_VARIANT="standard"
  fi
elif [ -z "$GOOSE_LINUX_VARIANT" ]; then
  GOOSE_LINUX_VARIANT="standard"
fi

# Debug output (safely handle undefined variables)
echo "WINDIR: ${WINDIR:-<not set>}"
echo "OSTYPE: $OSTYPE"
echo "uname -s: $(uname -s)"
echo "uname -m: $(uname -m)"
echo "PWD: $PWD"

# Output the detected OS
echo "Detected OS: $OS with ARCH $ARCH"

# Build the filename and URL for the stable release
if [ "$OS" = "darwin" ]; then
  FILE="goose-$ARCH-apple-darwin.tar.bz2"
  EXTRACT_CMD="tar"
elif [ "$OS" = "windows" ]; then
  case "$GOOSE_WINDOWS_VARIANT" in
    standard|cuda) ;;
    *)
      echo "Error: Unsupported GOOSE_WINDOWS_VARIANT '$GOOSE_WINDOWS_VARIANT'. Expected 'standard' or 'cuda'."
      exit 1
      ;;
  esac
  # Windows only supports x86_64 currently
  if [ "$ARCH" != "x86_64" ]; then
    echo "Error: Windows currently only supports x86_64 architecture."
    exit 1
  fi
  FILE="goose-$ARCH-pc-windows-msvc.zip"
  if [ "$GOOSE_WINDOWS_VARIANT" = "cuda" ]; then
    FILE="goose-$ARCH-pc-windows-msvc-cuda.zip"
  fi
  EXTRACT_CMD="unzip"
  OUT_FILE="goose.exe"
else
  case "$GOOSE_LINUX_VARIANT" in
    standard|vulkan|musl) ;;
    *)
      echo "Error: Unsupported GOOSE_LINUX_VARIANT '$GOOSE_LINUX_VARIANT'. Expected 'standard', 'vulkan', or 'musl'."
      exit 1
      ;;
  esac
  FILE="goose-$ARCH-unknown-linux-gnu.tar.bz2"
  if [ "$GOOSE_LINUX_VARIANT" = "vulkan" ]; then
    FILE="goose-$ARCH-unknown-linux-gnu-vulkan.tar.bz2"
  elif [ "$GOOSE_LINUX_VARIANT" = "musl" ]; then
    FILE="goose-$ARCH-unknown-linux-musl.tar.bz2"
  fi
  EXTRACT_CMD="tar"
fi

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$RELEASE_TAG/$FILE"

# --- 4) Download & extract 'goose' binary ---
TMP_BASE="${TMPDIR:-/tmp}"
if [ ! -d "$TMP_BASE" ]; then
  echo "Error: temporary directory does not exist: $TMP_BASE"
  exit 1
fi
TMP_DIR=$(mktemp -d "${TMP_BASE%/}/goose-install.XXXXXXXX") || {
  echo "Error: Could not create private temporary directory"
  exit 1
}
chmod 700 "$TMP_DIR"
ARCHIVE_PATH="$TMP_DIR/$FILE"

validate_tar_archive() {
  local archive=$1 member normalized member_count=0 goose_count=0 type_count=0 listing type_listing
  if ! listing=$(tar -tjf "$archive"); then
    echo "Error: Could not read archive member list"
    return 1
  fi
  while IFS= read -r member; do
    member_count=$((member_count + 1))
    normalized=${member#./}
    case "$normalized" in
      "") ;;
      goose|goose-package/goose) goose_count=$((goose_count + 1)) ;;
      goose-package|goose-package/) ;;
      *)
        echo "Error: Unexpected archive member: $member"
        return 1
        ;;
    esac
  done <<< "$listing"
  if [ "$member_count" -gt 16 ] || [ "$goose_count" -ne 1 ]; then
    echo "Error: Archive must contain exactly one goose binary"
    return 1
  fi
  if ! type_listing=$(tar -tvjf "$archive"); then
    echo "Error: Could not read archive member metadata"
    return 1
  fi
  while IFS= read -r metadata; do
    type_count=$((type_count + 1))
    case "${metadata:0:1}" in
      -|d) ;;
      *)
        echo "Error: Archive contains a link or special-file entry"
        return 1
        ;;
    esac
  done <<< "$type_listing"
  if [ "$type_count" -ne "$member_count" ]; then
    echo "Error: Could not validate every tar member type"
    return 1
  fi
}

validate_zip_archive() {
  local archive=$1 member normalized member_count=0 goose_count=0 type_count=0 entry_type
  while IFS= read -r member; do
    member_count=$((member_count + 1))
    normalized=${member//\\//}
    case "$normalized" in
      /*|*:*|../*|*/../*|*/..)
        echo "Error: Unsafe zip member path: $member"
        return 1
        ;;
      goose.exe|goose-package/goose.exe) goose_count=$((goose_count + 1)) ;;
      goose-package/*.dll)
        case "${normalized#goose-package/}" in
          */*) echo "Error: Unexpected nested DLL path: $member"; return 1 ;;
        esac
        ;;
      *.dll)
        case "$normalized" in
          */*) echo "Error: Unexpected DLL path: $member"; return 1 ;;
        esac
        ;;
      goose-package/|"") ;;
      *)
        echo "Error: Unexpected zip member: $member"
        return 1
        ;;
    esac
  done < <(unzip -Z -1 "$archive")
  if [ "$member_count" -gt 256 ] || [ "$goose_count" -ne 1 ]; then
    echo "Error: Zip archive must contain exactly one goose.exe"
    return 1
  fi
  while IFS= read -r entry_type; do
    type_count=$((type_count + 1))
    case "$entry_type" in
      -|d) ;;
      *)
        echo "Error: Zip archive contains a link or special-file entry"
        return 1
        ;;
    esac
  done < <(unzip -Z -l "$archive" | awk 'substr($0,1,1) ~ /^[-dlbcps]$/ { print substr($0,1,1) }')
  if [ "$type_count" -ne "$member_count" ]; then
    echo "Error: Could not validate every zip member type"
    return 1
  fi
}

echo "Downloading $RELEASE_TAG release: $FILE..."
if ! curl -sLf --retry 4 --retry-delay 2 --retry-max-time 120 \
  --connect-timeout 15 --max-time 600 --max-filesize 1073741824 \
  "$DOWNLOAD_URL" --output "$ARCHIVE_PATH"; then
  # If the download fails, only fall back to latest stable when no version was specified and canary was not requested).
  if ! [ -n "${GOOSE_VERSION:-}" ] && [ "${CANARY:-false}" != "true" ]; then
    LATEST_TAG=$(curl -fsSL --retry 4 --retry-delay 2 --retry-max-time 120 \
      --connect-timeout 15 --max-time 120 \
      "https://api.github.com/repos/$REPO/releases/latest" | \
      grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$LATEST_TAG" ]; then
      echo "Error: Failed to download $DOWNLOAD_URL and latest tag unavailable"
      exit 1
    fi

    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/$FILE"
    if curl -sLf --retry 4 --retry-delay 2 --retry-max-time 120 \
      --connect-timeout 15 --max-time 600 --max-filesize 1073741824 \
      "$DOWNLOAD_URL" --output "$ARCHIVE_PATH"; then
      # Fallback succeeded
      :
    else
      echo "Error: Failed to download from fallback url $DOWNLOAD_URL using latest tag $LATEST_TAG"
      exit 1
    fi
  else
    echo "Error: Failed to download $DOWNLOAD_URL"
    exit 1
  fi
fi

CHECKSUM_FILE="$FILE.sha256"
CHECKSUM_URL="$DOWNLOAD_URL.sha256"
CHECKSUM_PATH="$TMP_DIR/$CHECKSUM_FILE"
# This sidecar is an integrity check fetched from the same GitHub release. It
# does not independently authenticate a compromised release account. Installed
# Goose updates additionally require the fork's Sigstore/SLSA attestation.
echo "Downloading SHA-256 checksum: $CHECKSUM_FILE..."
if ! curl -sLf --retry 4 --retry-delay 2 --retry-max-time 120 \
  --connect-timeout 15 --max-time 120 --max-filesize 1024 \
  "$CHECKSUM_URL" --output "$CHECKSUM_PATH"; then
  echo "Error: Failed to download required checksum from $CHECKSUM_URL"
  exit 1
fi

read -r EXPECTED_SHA256 CHECKSUM_NAME CHECKSUM_EXTRA < "$CHECKSUM_PATH" || true
CHECKSUM_LINE_COUNT=$(wc -l < "$CHECKSUM_PATH" | tr -d '[:space:]')
if [ "$CHECKSUM_LINE_COUNT" != "1" ] || \
   [[ ! "${EXPECTED_SHA256:-}" =~ ^[0-9a-fA-F]{64}$ ]] || \
   [ "${CHECKSUM_NAME:-}" != "$FILE" ] || [ -n "${CHECKSUM_EXTRA:-}" ]; then
  echo "Error: Invalid checksum sidecar for $FILE"
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256=$(sha256sum "$ARCHIVE_PATH" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256=$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')
elif command -v openssl >/dev/null 2>&1; then
  ACTUAL_SHA256=$(openssl dgst -sha256 "$ARCHIVE_PATH" | awk '{print $NF}')
else
  echo "Error: SHA-256 verification requires sha256sum, shasum, or openssl"
  exit 1
fi

if [ "$(printf '%s' "$ACTUAL_SHA256" | tr '[:upper:]' '[:lower:]')" != \
     "$(printf '%s' "$EXPECTED_SHA256" | tr '[:upper:]' '[:lower:]')" ]; then
  echo "Error: SHA-256 checksum mismatch for $FILE; refusing to extract it"
  exit 1
fi
echo "Verified SHA-256 checksum for $FILE."
rm -f "$CHECKSUM_PATH"

ARCHIVE_SIZE=$(wc -c < "$ARCHIVE_PATH" | tr -d '[:space:]')
if [ "$ARCHIVE_SIZE" -gt 1073741824 ]; then
  echo "Error: Release archive exceeds the 1 GiB safety limit"
  exit 1
fi
if [ "$EXTRACT_CMD" = "tar" ]; then
  validate_tar_archive "$ARCHIVE_PATH"
else
  validate_zip_archive "$ARCHIVE_PATH"
fi

EXTRACT_DIR="$TMP_DIR/extract"
if ! mkdir "$EXTRACT_DIR"; then
  echo "Error: Could not create temporary extraction directory"
  exit 1
fi

echo "Extracting $FILE to temporary directory..."
set +e  # Disable immediate exit on error

if [ "$EXTRACT_CMD" = "tar" ]; then
  tar -xjf "$ARCHIVE_PATH" -C "$EXTRACT_DIR" 2> "$TMP_DIR/tar_error.log"
  extract_exit_code=$?

  # Check for tar errors
  if [ $extract_exit_code -ne 0 ]; then
    if grep -iEq "missing.*bzip2|bzip2.*missing|bzip2.*No such file|No such file.*bzip2" "$TMP_DIR/tar_error.log"; then
      echo "Error: Failed to extract $FILE. 'bzip2' is required but not installed. See details below:"
    else
      echo "Error: Failed to extract $FILE. See details below:"
    fi
    cat "$TMP_DIR/tar_error.log"
    exit 1
  fi
else
  # Use unzip for Windows
  unzip -q "$ARCHIVE_PATH" -d "$EXTRACT_DIR" 2> "$TMP_DIR/unzip_error.log"
  extract_exit_code=$?

  # Check for unzip errors
  if [ $extract_exit_code -ne 0 ]; then
    echo "Error: Failed to extract $FILE. See details below:"
    cat "$TMP_DIR/unzip_error.log"
    exit 1
  fi
fi

set -e  # Re-enable immediate exit on error

# Determine the extraction directory (handle subdirectory in Windows packages)
# Windows releases may contain files in a 'goose-package' subdirectory
if [ "$OS" = "windows" ] && [ -d "$EXTRACT_DIR/goose-package" ]; then
  echo "Found goose-package subdirectory, using that as extraction directory"
  EXTRACT_DIR="$EXTRACT_DIR/goose-package"
fi

# Make binary executable
if [ "$OS" = "windows" ]; then
  chmod +x "$EXTRACT_DIR/goose.exe"
else
  chmod +x "$EXTRACT_DIR/goose"
fi

# --- 5) Install to $GOOSE_BIN_DIR ---
if [ ! -d "$GOOSE_BIN_DIR" ]; then
  echo "Creating directory: $GOOSE_BIN_DIR"
  mkdir -p "$GOOSE_BIN_DIR"
fi

LOCK_DIR="$GOOSE_BIN_DIR/.goose-install.lock"
lock_attempt=0
while ! mkdir "$LOCK_DIR" 2>/dev/null; do
  lock_attempt=$((lock_attempt + 1))
  if [ -r "$LOCK_DIR/pid" ]; then
    read -r lock_pid < "$LOCK_DIR/pid" || lock_pid=""
    if [[ "$lock_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$lock_pid" 2>/dev/null; then
      stale_lock="$GOOSE_BIN_DIR/.goose-install.lock.stale.$$.$lock_attempt"
      if mv "$LOCK_DIR" "$stale_lock" 2>/dev/null; then
        rm -rf -- "$stale_lock"
        continue
      fi
    fi
  fi
  if [ "$lock_attempt" -ge 100 ]; then
    echo "Error: another goose installation is still promoting a binary; retry shortly"
    exit 1
  fi
  sleep 0.05
done
LOCK_HELD=true
printf '%s\n' "$$" > "$LOCK_DIR/pid"

if [ "$OS" != "windows" ]; then
  INSTALL_CANDIDATE=$(mktemp "$GOOSE_BIN_DIR/.${OUT_FILE}.install.XXXXXXXX") || {
    echo "Error: Could not stage goose in $GOOSE_BIN_DIR"
    exit 1
  }
  cp "$EXTRACT_DIR/goose" "$INSTALL_CANDIDATE"
  chmod +x "$INSTALL_CANDIDATE"
fi

echo "Moving goose to $GOOSE_BIN_DIR/$OUT_FILE"
if [ "$OS" = "windows" ]; then
  # Windows cannot atomically replace an executable plus its DLL set. Stage all
  # files beside the destination, remove the launchable executable first, then
  # promote DLLs and the executable last. Any failure restores the whole set.
  for stale_rollback in "$GOOSE_BIN_DIR"/.goose.rollback.*; do
    [ -d "$stale_rollback" ] || continue
    rm -rf -- "$stale_rollback" 2>/dev/null || true
  done
  WINDOWS_STAGE_DIR=$(mktemp -d "$GOOSE_BIN_DIR/.goose.stage.XXXXXXXX") || {
    echo "Error: Could not reserve Windows staging directory"
    exit 1
  }
  WINDOWS_ROLLBACK_DIR=$(mktemp -d "$GOOSE_BIN_DIR/.goose.rollback.XXXXXXXX") || {
    echo "Error: Could not reserve Windows rollback directory"
    exit 1
  }
  WINDOWS_MANIFEST="$TMP_DIR/windows-install-files"
  WINDOWS_PROMOTED_MANIFEST="$TMP_DIR/windows-promoted-files"
  : > "$WINDOWS_PROMOTED_MANIFEST"
  printf '%s\n' "goose.exe" > "$WINDOWS_MANIFEST"
  cp "$EXTRACT_DIR/goose.exe" "$WINDOWS_STAGE_DIR/goose.exe"
  chmod +x "$WINDOWS_STAGE_DIR/goose.exe"
  for dll in "$EXTRACT_DIR"/*.dll; do
    if [ -f "$dll" ]; then
      dll_name=$(basename "$dll")
      printf '%s\n' "$dll_name" >> "$WINDOWS_MANIFEST"
      cp "$dll" "$WINDOWS_STAGE_DIR/$dll_name"
    fi
  done

  while IFS= read -r install_name; do
    if [ -e "$GOOSE_BIN_DIR/$install_name" ] || [ -L "$GOOSE_BIN_DIR/$install_name" ]; then
      if ! mv -- "$GOOSE_BIN_DIR/$install_name" "$WINDOWS_ROLLBACK_DIR/$install_name"; then
        echo "Error: could not prepare Windows runtime transaction; restoring previous files"
        rollback_windows_install || true
        exit 1
      fi
    fi
  done < "$WINDOWS_MANIFEST"

  while IFS= read -r install_name; do
    [ "$install_name" = "goose.exe" ] && continue
    if ! mv -- "$WINDOWS_STAGE_DIR/$install_name" "$GOOSE_BIN_DIR/$install_name"; then
      echo "Error: failed to promote Windows runtime DLLs; restoring previous files"
      rollback_windows_install || true
      exit 1
    fi
    printf '%s\n' "$install_name" >> "$WINDOWS_PROMOTED_MANIFEST"
  done < "$WINDOWS_MANIFEST"
  if ! mv -- "$WINDOWS_STAGE_DIR/goose.exe" "$GOOSE_BIN_DIR/goose.exe"; then
    echo "Error: failed to promote goose.exe; restoring previous runtime"
    rollback_windows_install || true
    exit 1
  fi
  printf '%s\n' "goose.exe" >> "$WINDOWS_PROMOTED_MANIFEST"
  WINDOWS_COMMITTED=true
  rm -rf -- "$WINDOWS_STAGE_DIR" 2>/dev/null || true
  WINDOWS_STAGE_DIR=""
  rm -rf -- "$WINDOWS_ROLLBACK_DIR" 2>/dev/null || true
else
  # The candidate is already on the destination filesystem, so mv performs an
  # atomic rename and never opens/truncates a running executable or symlink.
  if ! mv -f "$INSTALL_CANDIDATE" "$GOOSE_BIN_DIR/$OUT_FILE"; then
    echo "Error: failed to atomically install new binary; previous version is unchanged"
    exit 1
  fi
  INSTALL_CANDIDATE=""
fi

release_install_lock

# skip configuration for non-interactive installs e.g. automation, docker
if [ "$CONFIGURE" = true ]; then
  # --- 6) Configure goose (Optional) ---
  echo ""
  echo "Configuring goose"
  echo ""
  if [ -t 0 ]; then
    "$GOOSE_BIN_DIR/$OUT_FILE" configure
  elif [ -r /dev/tty ]; then
    "$GOOSE_BIN_DIR/$OUT_FILE" configure < /dev/tty
  else
    echo "Non-interactive shell detected (e.g. 'curl ... | bash')."
    echo "Skipping 'goose configure' — please run it manually after installation:"
    echo "    $GOOSE_BIN_DIR/$OUT_FILE configure"
  fi
else
  echo "Skipping 'goose configure', you may need to run this manually later"
fi



# --- 7) Check PATH and give instructions if needed ---
if [[ ":$PATH:" != *":$GOOSE_BIN_DIR:"* ]]; then
  echo ""
  echo "Warning: goose installed, but $GOOSE_BIN_DIR is not in your PATH."

  if [ "$OS" = "windows" ]; then
    echo "To add goose to your PATH in PowerShell:"
    echo ""
    echo "# Add to your PowerShell profile"
    echo '$profilePath = $PROFILE'
    echo 'if (!(Test-Path $profilePath)) { New-Item -Path $profilePath -ItemType File -Force }'
    echo 'Add-Content -Path $profilePath -Value ''$env:PATH = "$env:USERPROFILE\.local\bin;$env:PATH"'''
    echo "# Reload profile or restart PowerShell"
    echo '. $PROFILE'
    echo ""
    echo "Alternatively, you can run:"
    echo "    goose configure"
    echo "or rerun this install script after updating your PATH."
  else
    SHELL_NAME=$(basename "$SHELL")

    echo ""
    echo "The \$GOOSE_BIN_DIR is not in your PATH."

    if [ "$CONFIGURE" = true ]; then
      echo "What would you like to do?"
      echo "1) Add it for me"
      echo "2) I'll add it myself, show instructions"

      # Check whether stdin is a terminal. If it is not (for example, if
      # this script has been piped into bash), we need to explicitly read user's
      # choice from /dev/tty.
      if [ -t 0 ]; then # terminal
        read -p "Enter choice [1/2]: " choice
      elif [ -r /dev/tty ]; then # not a terminal, but /dev/tty is available
        read -p "Enter choice [1/2]: " choice < /dev/tty
      else # non-interactive environment without /dev/tty
        echo "Non-interactive environment detected without /dev/tty; defaulting to option 2 (show instructions)."
        choice=2
      fi

      case "$choice" in
      1)
        RC_FILE="$HOME/.${SHELL_NAME}rc"
        echo "Adding \$GOOSE_BIN_DIR to $RC_FILE..."
        echo "export PATH=\"$GOOSE_BIN_DIR:\$PATH\"" >> "$RC_FILE"
        echo "Done! Reload your shell or run 'source $RC_FILE' to apply changes."
        ;;
      2)
        echo ""
        echo "Add it to your PATH by editing ~/.${SHELL_NAME}rc or similar:"
        echo "    export PATH=\"$GOOSE_BIN_DIR:\$PATH\""
        echo "Then reload your shell (e.g. 'source ~/.${SHELL_NAME}rc') to apply changes."
        ;;
      *)
        echo "Invalid choice. Please add \$GOOSE_BIN_DIR to your PATH manually."
        ;;
      esac
    else
      echo ""
      echo "Configure disabled. Please add \$GOOSE_BIN_DIR to your PATH manually."
    fi

  fi

  echo ""
fi
