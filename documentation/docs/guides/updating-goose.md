---
sidebar_position: 6
title: Updating goose
sidebar_label: Updating goose
---

import { DesktopAutoUpdateSteps } from '@site/src/components/DesktopAutoUpdateSteps';
import MacDesktopInstallButtons from '@site/src/components/MacDesktopInstallButtons';
import WindowsDesktopInstallButtons from '@site/src/components/WindowsDesktopInstallButtons';
import LinuxDesktopInstallButtons from '@site/src/components/LinuxDesktopInstallButtons';

# Updating goose

## ViaTech CLI

:::warning Use the ViaTech release channel
The ViaTech CLI does not update itself in the background. The minimal source
build below does not include `goose update`; packaged ViaTech builds do, and
their updater is pinned to the ViaTech release channel. Upstream package
managers and desktop downloads remain separate distribution channels.
:::

### Source installs made before the first binary release

If you installed from source before `stable` appeared on the
[ViaTech releases page](https://github.com/ViaTechSystems/goose/releases),
update by rerunning the same Cargo command used to install it:

```bash
cargo install --force --git https://github.com/ViaTechSystems/goose goose-cli \
  --locked --no-default-features --features rustls-tls,code-mode
goose --version
```

This requires Rust/Cargo and follows the current ViaTech branch. For a
reproducible source build, add `--rev <full-commit-sha>` before `goose-cli`.

### Packaged installs from the `stable` release

Rerun the ViaTech installer to install or update:

```bash
curl -fsSL https://github.com/ViaTechSystems/goose/releases/download/stable/download_cli.sh | bash
goose --version
```

The installer supports macOS, Linux, WSL, Android/Termux, Git Bash, and MSYS2.
It downloads from `ViaTechSystems/goose`, requires a one-line SHA-256 sidecar
naming the exact archive, stages in a private directory, validates archive
members and size limits, and verifies the archive before extraction. A
missing, malformed, or mismatched checksum stops the install. Native PowerShell
does not yet have a checksum-backed ViaTech installer; use the Cargo command
there.

To pin an already-published release for CI or reproducible local installs:

```bash
GOOSE_VERSION=vX.Y.Z CONFIGURE=false \
  curl -fsSL https://github.com/ViaTechSystems/goose/releases/download/stable/download_cli.sh | bash
```

`GOOSE_BIN_DIR` changes the destination. The default is `~/.local/bin` on
macOS/Linux/WSL and `%USERPROFILE%\\goose` in Git Bash or MSYS2 on native
Windows.

A packaged ViaTech build can instead update itself on demand:

```bash
# Latest stable ViaTech release
goose update

# Latest ViaTech canary release
goose update --canary

# Re-run configuration after a verified update
goose update --reconfigure
```

This is a foreground command, not an automatic update. It downloads the
platform archive from `ViaTechSystems/goose` and refuses to replace the current
executable unless the archive passes Sigstore/SLSA provenance verification.
The replacement is staged beside the installed executable and committed as an
atomic rename on Unix. Native Windows uses a destination lock and a rollback
transaction for the executable and runtime DLLs.

### Fresh CLI reinstall

A reinstall normally needs to replace only the executable. It does not require
deleting sessions, configuration, or secrets.

For a Cargo installation:

```bash
cargo uninstall goose-cli
cargo install --force --git https://github.com/ViaTechSystems/goose goose-cli \
  --locked --no-default-features --features rustls-tls,code-mode
```

For a checksum-installer installation, remove the old executable from the
installer destination and rerun the installer. With the Unix default:

```bash
rm -f "$HOME/.local/bin/goose"
curl -fsSL https://github.com/ViaTechSystems/goose/releases/download/stable/download_cli.sh | bash
```

If you used `GOOSE_BIN_DIR`, remove only `goose` (or `goose.exe`) from that
exact directory. See [Uninstall goose or remove cached data](/docs/troubleshooting/known-issues#uninstall-goose-or-remove-cached-data)
only when you deliberately want to erase local state as well.

## Upstream desktop app

The desktop downloads currently remain upstream AAIF builds and do not include
the ViaTech terminal coding-session controls. Their update UI is separate from
the ViaTech CLI and does not update a CLI installation.

### Automatic update setting

<DesktopAutoUpdateSteps />

### Manual desktop update

- **macOS:** Download with <MacDesktopInstallButtons/>, unzip it, and replace
  `Goose.app` in `Applications`.
- **Linux:** Download with <LinuxDesktopInstallButtons/>. On Debian or Ubuntu,
  install the downloaded package with `sudo dpkg -i <filename>.deb`.
- **Windows:** Download with <WindowsDesktopInstallButtons/>, unzip it, and run
  the executable.

:::info CI/CD
Before the first binary release, pin a full Git commit with Cargo. After the
release, set `GOOSE_VERSION` and use the checksum-backed installer. See
[CI/CD Environments](/docs/tutorials/cicd) for additional non-interactive
setup guidance.
:::
