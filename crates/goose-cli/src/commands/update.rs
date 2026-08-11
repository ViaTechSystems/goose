use anyhow::{bail, Context, Result};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    StatusCode,
};
use sha2::{Digest, Sha256};
use sigstore_verify::trust_root::{TrustedRoot, SIGSTORE_PRODUCTION_TRUSTED_ROOT};
use sigstore_verify::types::{Bundle, Sha256Hash};
use sigstore_verify::VerificationPolicy;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_REPOSITORY: &str = "ViaTechSystems/goose";
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 256;

fn attestation_url(digest: &str) -> String {
    format!(
        "https://api.github.com/repos/{RELEASE_REPOSITORY}/attestations/sha256:{digest}?per_page=30&predicate_type=https://slsa.dev/provenance/v1"
    )
}

fn release_asset_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{RELEASE_REPOSITORY}/releases/download/{tag}/{asset}")
}

/// Asset name for this platform (compile-time).
fn asset_name() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "goose-aarch64-apple-darwin.tar.bz2"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "goose-x86_64-apple-darwin.tar.bz2"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        "goose-x86_64-unknown-linux-gnu.tar.bz2"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        "goose-aarch64-unknown-linux-gnu.tar.bz2"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        "goose-x86_64-unknown-linux-musl.tar.bz2"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "musl"))]
    {
        "goose-aarch64-unknown-linux-musl.tar.bz2"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64", feature = "cuda"))]
    {
        "goose-x86_64-pc-windows-msvc-cuda.zip"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64", not(feature = "cuda")))]
    {
        "goose-x86_64-pc-windows-msvc.zip"
    }
}

/// Binary name for this platform.
fn binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "goose.exe"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "goose"
    }
}

// ---------------------------------------------------------------------------
// Sigstore / SLSA provenance verification
// ---------------------------------------------------------------------------

/// Compute the SHA-256 hex digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    goose::utils::bytes_to_hex(hasher.finalize())
}

#[derive(serde::Deserialize)]
struct AttestationResponse {
    attestations: Vec<AttestationEntry>,
}

#[derive(serde::Deserialize)]
struct AttestationEntry {
    bundle: serde_json::Value,
}

const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";

fn workflow_identity_matches(identity: &str, workflow: &str) -> bool {
    let expected =
        format!("https://github.com/{RELEASE_REPOSITORY}/.github/workflows/{workflow}@refs/");
    identity.starts_with(&expected)
}

fn sanitized_token(token: Option<&str>) -> Option<&str> {
    token.map(str::trim).filter(|tok| !tok.is_empty())
}

fn authorization_header_value(token: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("Bearer {token}")).ok()
}

fn github_token() -> Option<String> {
    env::var("GITHUB_TOKEN")
        .ok()
        .and_then(|tok| sanitized_token(Some(&tok)).map(str::to_owned))
        .or_else(|| {
            env::var("GH_TOKEN")
                .ok()
                .and_then(|tok| sanitized_token(Some(&tok)).map(str::to_owned))
        })
}

fn should_retry_attestations_without_token(status: StatusCode, token: Option<&str>) -> bool {
    sanitized_token(token)
        .and_then(authorization_header_value)
        .is_some()
        && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

async fn fetch_attestations(digest: &str, token: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let url = attestation_url(digest);

    let client = reqwest::Client::new();
    let token = sanitized_token(token);
    let resp = fetch_attestations_response(&client, &url, token).await?;

    let resp = if should_retry_attestations_without_token(resp.status(), token) {
        fetch_attestations_response(&client, &url, None).await?
    } else {
        resp
    };

    if !resp.status().is_success() {
        bail!("GitHub attestation API returned HTTP {}", resp.status());
    }

    let body: AttestationResponse = resp
        .json()
        .await
        .context("Failed to parse attestation response")?;

    Ok(body.attestations.into_iter().map(|a| a.bundle).collect())
}

async fn fetch_attestations_response(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::Response> {
    let mut req = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "goose-cli");

    if let Some(value) = token.and_then(authorization_header_value) {
        req = req.header(AUTHORIZATION, value);
    }

    req.send().await.context("Failed to fetch attestations")
}

// Verify a single attestation bundle against the artifact digest and workflow.
fn verify_bundle(
    bundle_json: &serde_json::Value,
    artifact_digest: Sha256Hash,
    policy: &VerificationPolicy,
    trusted_root: &TrustedRoot,
    workflow: &str,
) -> Result<()> {
    let bundle_str = serde_json::to_string(bundle_json)?;
    let bundle = Bundle::from_json(&bundle_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

    let result = sigstore_verify::verify(artifact_digest, &bundle, policy, trusted_root)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if !result.success {
        bail!("Verification unsuccessful");
    }

    let identity = result
        .identity
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No identity in certificate"))?;

    if !workflow_identity_matches(identity, workflow) {
        bail!("Workflow identity mismatch for {workflow}: got {identity}");
    }

    Ok(())
}

/// Returns `Ok(())` when the downloaded archive has verified provenance.
async fn verify_provenance(archive_data: &[u8], tag: &str) -> Result<()> {
    let digest = sha256_hex(archive_data);
    println!("Archive SHA-256: {digest}");

    let workflow = match tag {
        "canary" => "canary.yml",
        _ => "release.yml",
    };

    let token = github_token();

    println!("Verifying SLSA provenance via Sigstore...");

    let bundles = fetch_attestations(&digest, token.as_deref())
        .await
        .context(
            "Sigstore provenance check could not complete; refusing to install unverifiable update",
        )?;

    if bundles.is_empty() {
        bail!("No Sigstore attestation found for downloaded archive; refusing to install unverifiable update");
    }

    let trusted_root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
        .context("Failed to load Sigstore trusted root")?;
    let policy = VerificationPolicy::with_issuer(GITHUB_ACTIONS_ISSUER);
    let artifact_digest =
        Sha256Hash::from_hex(&digest).context("Failed to parse artifact digest")?;

    // One passing attestation is sufficient.
    let mut last_err = None;
    for bundle_json in &bundles {
        match verify_bundle(
            bundle_json,
            artifact_digest,
            &policy,
            &trusted_root,
            workflow,
        ) {
            Ok(()) => {
                println!("Sigstore provenance verification passed.");
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(anyhow::anyhow!(
        "Sigstore verification failed: {}\n\nAborting update due to security check failure.",
        last_err.unwrap()
    ))
}

/// Update the goose binary to the latest release.
///
/// Downloads the platform-appropriate archive from GitHub releases, verifies
/// its SLSA provenance via Sigstore, extracts it with path-traversal
/// hardening, and replaces the current binary in-place.
pub async fn update(canary: bool, reconfigure: bool) -> Result<()> {
    #[cfg(feature = "disable-update")]
    {
        bail!("Update is disabled in this build.");
    }

    #[cfg(not(feature = "disable-update"))]
    {
        let tag = if canary { "canary" } else { "stable" };
        let asset = asset_name();
        let url = release_asset_url(tag, asset);

        println!("Downloading {asset} from {tag} release...");

        // --- Download -----------------------------------------------------------
        let mut response = reqwest::get(&url)
            .await
            .context("Failed to download release archive")?;

        if !response.status().is_success() {
            bail!(
                "Download failed with HTTP status {}. URL: {}",
                response.status(),
                url
            );
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
        {
            bail!("Release archive exceeds the 1 GiB safety limit");
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Failed to read response body")?
        {
            let next_size = bytes
                .len()
                .checked_add(chunk.len())
                .context("Release archive size overflow")?;
            if next_size as u64 > MAX_ARCHIVE_BYTES {
                bail!("Release archive exceeds the 1 GiB safety limit");
            }
            bytes.extend_from_slice(&chunk);
        }

        println!("Downloaded {} bytes.", bytes.len());

        // --- Verify SLSA provenance via Sigstore --------------------------------
        verify_provenance(&bytes, tag).await?;

        // --- Extract to temp dir (hardened against path traversal) --------------
        let tmp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

        #[cfg(target_os = "windows")]
        extract_zip(&bytes, tmp_dir.path())?;

        #[cfg(not(target_os = "windows"))]
        extract_tar_bz2(&bytes, tmp_dir.path())?;

        // --- Locate the binary in the extracted archive -------------------------
        let binary = binary_name();
        let extracted_binary = find_binary(tmp_dir.path(), binary)
            .with_context(|| format!("Could not find {binary} in extracted archive"))?;

        // --- Replace the current binary -----------------------------------------
        let current_exe =
            env::current_exe().context("Failed to determine current executable path")?;

        #[cfg(target_os = "windows")]
        replace_windows_installation(&extracted_binary, &current_exe)
            .context("Failed to replace Windows runtime")?;

        #[cfg(not(target_os = "windows"))]
        replace_binary(&extracted_binary, &current_exe)
            .context("Failed to replace current binary")?;

        println!("goose updated successfully (verified with Sigstore SLSA provenance).");

        // --- Reconfigure if requested -------------------------------------------
        if reconfigure {
            println!("Running goose configure...");
            let status = Command::new(current_exe)
                .arg("configure")
                .status()
                .context("Failed to run goose configure")?;
            if !status.success() {
                eprintln!("Warning: goose configure exited with {status}");
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Archive extraction
// ---------------------------------------------------------------------------

/// Extract a .zip archive with path-traversal hardening (Windows).
///
/// Iterates entries individually and uses `enclosed_name()` to reject any
/// path that escapes the destination directory (zip-slip protection).
#[cfg(target_os = "windows")]
fn extract_zip(data: &[u8], dest: &Path) -> Result<()> {
    use std::io::Cursor;
    let cursor = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open zip archive")?;

    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("Zip archive contains too many entries");
    }
    let mut extracted_bytes = 0_u64;
    let mut binary_count = 0_usize;
    let mut normalized_names = std::collections::HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("Failed to read zip entry at index {i}"))?;

        let safe_path = match entry.enclosed_name() {
            Some(p) => p.to_owned(),
            None => bail!("Zip entry has unsafe path: {}", entry.name()),
        };

        let components: Vec<_> = safe_path.components().collect();
        let file_name = safe_path.file_name().and_then(|name| name.to_str());
        let allowed = entry.is_dir() && components.len() == 1 && file_name == Some("goose-package")
            || !entry.is_dir()
                && (components.len() == 1
                    || components.len() == 2 && components[0].as_os_str() == "goose-package")
                && file_name.is_some_and(|name| {
                    name.eq_ignore_ascii_case("goose.exe")
                        || name.to_ascii_lowercase().ends_with(".dll")
                });
        if !allowed {
            bail!("Unexpected zip archive member: {}", safe_path.display());
        }
        let normalized_name = safe_path.to_string_lossy().to_ascii_lowercase();
        if !normalized_names.insert(normalized_name) {
            bail!("Zip archive contains duplicate case-insensitive member names");
        }
        if file_name.is_some_and(|name| name.eq_ignore_ascii_case("goose.exe")) {
            binary_count += 1;
        }
        if let Some(mode) = entry.unix_mode() {
            let file_type = mode & 0o170000;
            let expected_type = if entry.is_dir() { 0o040000 } else { 0o100000 };
            if file_type != 0 && file_type != expected_type {
                bail!("Zip archive contains a link or special-file entry");
            }
        }
        extracted_bytes = extracted_bytes
            .checked_add(entry.size())
            .context("Zip expanded-size overflow")?;
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            bail!("Zip archive exceeds the 4 GiB expanded-size safety limit");
        }

        let target = dest.join(&safe_path);

        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&target)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }

    if binary_count != 1 {
        bail!("Zip archive must contain exactly one goose.exe");
    }

    Ok(())
}

/// Validate that an archive entry path is safe (no absolute paths, no `..`).
fn validate_entry_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        bail!("Tar entry has absolute path: {}", path.display());
    }
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            bail!("Tar entry contains path traversal: {}", path.display());
        }
    }
    Ok(())
}

/// Extract a .tar.bz2 archive with path-traversal hardening (macOS / Linux).
///
/// Iterates entries individually, rejecting any entry whose path is absolute
/// or contains `..` components (tar-slip protection).
#[cfg(not(target_os = "windows"))]
fn extract_tar_bz2(data: &[u8], dest: &Path) -> Result<()> {
    use bzip2::read::BzDecoder;
    let decoder = BzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    let mut entry_count = 0_usize;
    let mut extracted_bytes = 0_u64;
    let mut binary_count = 0_usize;
    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read tar entry")?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            bail!("Tar archive contains too many entries");
        }
        let path = entry
            .path()
            .context("Failed to read entry path")?
            .into_owned();

        validate_entry_path(&path)?;

        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            bail!("Tar archive contains a link or special-file entry");
        }
        let normal_components: Vec<_> = path
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .collect();
        if entry_type.is_file() {
            let allowed_binary = normal_components.len() == 1
                && normal_components[0].as_os_str() == "goose"
                || normal_components.len() == 2
                    && normal_components[0].as_os_str() == "goose-package"
                    && normal_components[1].as_os_str() == "goose";
            if !allowed_binary {
                bail!("Unexpected tar archive member: {}", path.display());
            }
            binary_count += 1;
        } else if !(normal_components.is_empty()
            || normal_components.len() == 1 && normal_components[0].as_os_str() == "goose-package")
        {
            bail!("Unexpected tar archive directory: {}", path.display());
        }
        extracted_bytes = extracted_bytes
            .checked_add(entry.header().size().context("Invalid tar entry size")?)
            .context("Tar expanded-size overflow")?;
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            bail!("Tar archive exceeds the 4 GiB expanded-size safety limit");
        }

        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        entry
            .unpack(&target)
            .with_context(|| format!("Failed to extract: {}", path.display()))?;
    }

    if binary_count != 1 {
        bail!("Tar archive must contain exactly one goose binary");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Binary location
// ---------------------------------------------------------------------------

/// Find the binary inside the extracted archive.
///
/// The archive may place it in:
///   1. A `goose-package/` subdirectory (Windows releases)
///   2. Directly at the top level
///   3. In some other single subdirectory
fn find_binary(extract_dir: &Path, binary_name: &str) -> Option<PathBuf> {
    // 1. Check goose-package subdir (matches download_cli.sh / download_cli.ps1)
    let package_dir = extract_dir.join("goose-package");
    if package_dir.is_dir() {
        let p = package_dir.join(binary_name);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. Check top level
    let p = extract_dir.join(binary_name);
    if p.exists() {
        return Some(p);
    }

    // 3. Search one level of subdirectories
    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let candidate = entry.path().join(binary_name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Binary replacement
// ---------------------------------------------------------------------------

/// Replace the current binary with the newly downloaded one.
///
/// Stage the replacement beside the current executable so the final rename is
/// same-filesystem and atomic. On Windows we must first rename the running exe
/// to a unique rollback path because Windows does not replace locked files.
fn replace_binary(new_binary: &Path, current_exe: &Path) -> Result<()> {
    let install_dir = current_exe
        .parent()
        .context("Current executable has no parent directory")?;
    let staged = tempfile::Builder::new()
        .prefix(".goose-update-")
        .tempfile_in(install_dir)
        .context("Failed to reserve same-filesystem update staging file")?;
    fs::copy(new_binary, staged.path()).with_context(|| {
        format!(
            "Failed to stage new binary beside {}",
            current_exe.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(staged.path())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(staged.path(), perms)?;
    }

    staged
        .as_file()
        .sync_all()
        .context("Failed to flush staged update binary")?;
    let (_, staged_path) = staged
        .keep()
        .map_err(|e| e.error)
        .context("Failed to retain staged update binary")?;

    let result = promote_staged_binary(&staged_path, current_exe);
    if result.is_err() {
        let _ = fs::remove_file(&staged_path);
    }
    result
}

fn promote_staged_binary(staged_path: &Path, current_exe: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let install_dir = current_exe
            .parent()
            .context("Current executable has no parent directory")?;
        // Best-effort migration cleanup for the fixed backup name used by
        // older versions. A currently running image remains locked on Windows.
        let _ = fs::remove_file(current_exe.with_extension("exe.old"));
        let backup = tempfile::Builder::new()
            .prefix(".goose-rollback-")
            .tempfile_in(install_dir)
            .context("Failed to reserve a unique rollback path")?;
        let (_, old_exe) = backup
            .keep()
            .map_err(|e| e.error)
            .context("Failed to retain rollback path")?;
        fs::remove_file(&old_exe).context("Failed to prepare rollback path")?;

        // Rename the running binary out of the way
        let had_current = current_exe.exists();
        if had_current {
            fs::rename(current_exe, &old_exe).with_context(|| {
                format!(
                    "Failed to rename running binary to {}. Try closing Goose Desktop if it's open.",
                    old_exe.display()
                )
            })?;
        }

        if let Err(error) = fs::rename(staged_path, current_exe) {
            if had_current {
                let _ = fs::rename(&old_exe, current_exe);
            }
            return Err(error)
                .with_context(|| format!("Failed to promote {}", current_exe.display()));
        }

        if had_current {
            let _ = fs::remove_file(&old_exe);
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(staged_path, current_exe).with_context(|| {
            format!(
                "Failed to atomically promote update to {}",
                current_exe.display()
            )
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Transactional Windows runtime replacement
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn replace_windows_installation(extracted_binary: &Path, current_exe: &Path) -> Result<()> {
    use std::ffi::OsString;

    let source_dir = extracted_binary
        .parent()
        .context("Extracted binary has no parent directory")?;
    let install_dir = current_exe
        .parent()
        .context("Current executable has no parent directory")?;

    let lock_path = install_dir.join(".goose-update.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .context("Failed to open update lock")?;
    lock.try_lock()
        .context("Another goose update is already promoting a Windows runtime")?;

    let stage_temp = tempfile::Builder::new()
        .prefix(".goose-stage-")
        .tempdir_in(install_dir)
        .context("Failed to create same-filesystem Windows staging directory")?;
    let stage_dir = stage_temp.path().to_path_buf();
    let rollback_temp = tempfile::Builder::new()
        .prefix(".goose-rollback-")
        .tempdir_in(install_dir)
        .context("Failed to create Windows rollback directory")?;
    let rollback_dir = rollback_temp.path().to_path_buf();

    let binary_name = current_exe
        .file_name()
        .context("Current executable has no filename")?
        .to_os_string();
    let mut names = vec![binary_name.clone()];
    fs::copy(extracted_binary, stage_dir.join(&binary_name))
        .context("Failed to stage goose.exe")?;

    for entry in fs::read_dir(source_dir).context("Failed to read extracted Windows runtime")? {
        let entry = entry.context("Failed to read extracted Windows runtime entry")?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
        {
            let name = entry.file_name();
            fs::copy(&path, stage_dir.join(&name))
                .with_context(|| format!("Failed to stage Windows runtime {}", path.display()))?;
            names.push(name);
        }
    }
    names[1..].sort_by_key(|name| name.to_string_lossy().to_ascii_lowercase());

    let mut backed_up: Vec<OsString> = Vec::new();
    let mut promoted: Vec<OsString> = Vec::new();
    let rollback = |promoted: &[OsString], backed_up: &[OsString]| -> Result<()> {
        for name in promoted.iter().rev() {
            let _ = fs::remove_file(install_dir.join(name));
        }
        for name in backed_up.iter().rev() {
            fs::rename(rollback_dir.join(name), install_dir.join(name)).with_context(|| {
                format!(
                    "Rollback failed for {}; recovery files remain in {}",
                    name.to_string_lossy(),
                    rollback_dir.display()
                )
            })?;
        }
        Ok(())
    };

    // Remove the launchable executable first so a new process cannot observe a
    // partially replaced DLL set. Promote all DLLs, then goose.exe last.
    for name in &names {
        let target = install_dir.join(name);
        if target.exists() {
            if let Err(error) = fs::rename(&target, rollback_dir.join(name)) {
                if let Err(rollback_error) = rollback(&promoted, &backed_up) {
                    let recovery_path = rollback_temp.keep();
                    return Err(rollback_error).with_context(|| {
                        format!(
                            "Recovery files were preserved in {}",
                            recovery_path.display()
                        )
                    });
                }
                return Err(error).with_context(|| {
                    format!(
                        "Failed to prepare Windows runtime file {}",
                        target.display()
                    )
                });
            }
            backed_up.push(name.clone());
        }
    }

    for name in names.iter().skip(1).chain(std::iter::once(&binary_name)) {
        if let Err(error) = fs::rename(stage_dir.join(name), install_dir.join(name)) {
            if let Err(rollback_error) = rollback(&promoted, &backed_up) {
                let recovery_path = rollback_temp.keep();
                return Err(rollback_error).with_context(|| {
                    format!(
                        "Recovery files were preserved in {}",
                        recovery_path.display()
                    )
                });
            }
            return Err(error).with_context(|| {
                format!(
                    "Failed to promote Windows runtime {}",
                    name.to_string_lossy()
                )
            });
        }
        promoted.push(name.clone());
    }

    drop(stage_temp);
    drop(rollback_temp);
    drop(lock);
    let _ = fs::remove_file(lock_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_asset_name_valid() {
        let name = asset_name();
        assert!(!name.is_empty());
        assert!(name.starts_with("goose-"));
        #[cfg(target_os = "windows")]
        assert!(name.ends_with(".zip"));
        #[cfg(not(target_os = "windows"))]
        assert!(name.ends_with(".tar.bz2"));
    }

    #[test]
    fn test_binary_name() {
        let name = binary_name();
        #[cfg(target_os = "windows")]
        assert_eq!(name, "goose.exe");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(name, "goose");
    }

    #[test]
    fn update_channel_is_pinned_to_the_viatech_fork() {
        assert_eq!(RELEASE_REPOSITORY, "ViaTechSystems/goose");
        assert_eq!(
            release_asset_url("stable", "goose-test.tar.bz2"),
            "https://github.com/ViaTechSystems/goose/releases/download/stable/goose-test.tar.bz2"
        );
        let url = attestation_url("abc123");
        assert!(url.starts_with(
            "https://api.github.com/repos/ViaTechSystems/goose/attestations/sha256:abc123"
        ));
        assert!(!url.contains("aaif-goose"));
    }

    #[test]
    fn provenance_identity_is_bound_to_fork_workflow_and_ref() {
        assert!(workflow_identity_matches(
            "https://github.com/ViaTechSystems/goose/.github/workflows/canary.yml@refs/heads/main",
            "canary.yml"
        ));
        assert!(workflow_identity_matches(
            "https://github.com/ViaTechSystems/goose/.github/workflows/release.yml@refs/tags/v1.46.0",
            "release.yml"
        ));
        assert!(!workflow_identity_matches(
            "https://github.com/attacker/goose/.github/workflows/canary.yml@refs/heads/main",
            "canary.yml"
        ));
        assert!(!workflow_identity_matches(
            "https://github.com/ViaTechSystems/goose/.github/workflows/not-canary.yml@refs/heads/main",
            "canary.yml"
        ));
    }

    #[test]
    fn test_find_binary_in_package_subdir() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("goose-package");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join(binary_name()), b"fake").unwrap();

        let found = find_binary(tmp.path(), binary_name());
        assert!(found.is_some());
        assert!(found.unwrap().ends_with(binary_name()));
    }

    #[test]
    fn test_find_binary_top_level() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join(binary_name()), b"fake").unwrap();

        let found = find_binary(tmp.path(), binary_name());
        assert!(found.is_some());
        assert_eq!(found.unwrap(), tmp.path().join(binary_name()));
    }

    #[test]
    fn test_find_binary_nested_subdir() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("some-dir");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join(binary_name()), b"fake").unwrap();

        let found = find_binary(tmp.path(), binary_name());
        assert!(found.is_some());
    }

    #[test]
    fn test_find_binary_not_found() {
        let tmp = tempdir().unwrap();
        let found = find_binary(tmp.path(), binary_name());
        assert!(found.is_none());
    }

    #[test]
    fn test_replace_binary_basic() {
        let tmp = tempdir().unwrap();
        let new_bin = tmp.path().join("new_goose");
        let current = tmp.path().join("current_goose");

        fs::write(&new_bin, b"new version").unwrap();
        fs::write(&current, b"old version").unwrap();

        replace_binary(&new_bin, &current).unwrap();

        let content = fs::read_to_string(&current).unwrap();
        assert_eq!(content, "new version");
        assert!(fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".goose-")));
    }

    #[cfg(unix)]
    #[test]
    fn replace_binary_replaces_symlink_without_clobbering_its_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempdir().unwrap();
        let victim = tmp.path().join("victim");
        let current = tmp.path().join("goose");
        let new_bin = tmp.path().join("new_goose");
        fs::write(&victim, b"do not overwrite").unwrap();
        fs::write(&new_bin, b"new version").unwrap();
        symlink(&victim, &current).unwrap();

        replace_binary(&new_bin, &current).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"do not overwrite");
        assert_eq!(fs::read(&current).unwrap(), b"new version");
        assert!(!fs::symlink_metadata(&current)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_replacements_leave_one_complete_binary_and_no_staging_files() {
        use std::sync::{Arc, Barrier};

        let tmp = Arc::new(tempdir().unwrap());
        let current = tmp.path().join("goose");
        fs::write(&current, b"old").unwrap();
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();

        for index in 0..8 {
            let tmp = Arc::clone(&tmp);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                let candidate = tmp.path().join(format!("candidate-{index}"));
                let payload = format!("complete-{index}");
                fs::write(&candidate, payload.as_bytes()).unwrap();
                barrier.wait();
                replace_binary(&candidate, &tmp.path().join("goose")).unwrap();
            }));
        }

        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let installed = fs::read_to_string(&current).unwrap();
        assert!((0..8).any(|index| installed == format!("complete-{index}")));
        assert!(fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".goose-")));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_replace_binary_windows_rename_away() {
        let tmp = tempdir().unwrap();
        let current = tmp.path().join("goose.exe");
        let new_bin = tmp.path().join("new_goose.exe");

        fs::write(&current, b"old version").unwrap();
        fs::write(&new_bin, b"new version").unwrap();

        replace_binary(&new_bin, &current).unwrap();

        // Current should now have new content
        let content = fs::read_to_string(&current).unwrap();
        assert_eq!(content, "new version");

        // Successful staging does not leave the legacy fixed backup behind.
        let old = current.with_extension("exe.old");
        assert!(!old.exists());
        assert!(fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".goose-")));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_replace_binary_windows_cleanup_old() {
        let tmp = tempdir().unwrap();
        let current = tmp.path().join("goose.exe");
        let old = current.with_extension("exe.old");
        let new_bin = tmp.path().join("new_goose.exe");

        // Simulate a previous update left .old behind
        fs::write(&current, b"version 2").unwrap();
        fs::write(&old, b"version 1").unwrap();
        fs::write(&new_bin, b"version 3").unwrap();

        replace_binary(&new_bin, &current).unwrap();

        let content = fs::read_to_string(&current).unwrap();
        assert_eq!(content, "version 3");

        // The legacy fixed rollback path is retired and cleaned up.
        assert!(!old.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_extract_zip_with_package_dir() {
        use std::io::Cursor;
        use std::io::Write;

        let tmp = tempdir().unwrap();

        // Create a zip in memory with goose-package/ structure
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            writer.add_directory("goose-package/", options).unwrap();
            writer
                .start_file("goose-package/goose.exe", options)
                .unwrap();
            writer.write_all(b"fake goose binary").unwrap();
            writer
                .start_file("goose-package/libtest.dll", options)
                .unwrap();
            writer.write_all(b"fake dll").unwrap();
            writer.finish().unwrap();
        }

        extract_zip(&buf, tmp.path()).unwrap();

        let binary = find_binary(tmp.path(), "goose.exe");
        assert!(binary.is_some());

        let content = fs::read_to_string(binary.unwrap()).unwrap();
        assert_eq!(content, "fake goose binary");

        // DLL should be in goose-package too
        assert!(tmp.path().join("goose-package/libtest.dll").exists());
    }

    // -----------------------------------------------------------------------
    // SHA-256 digest tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sha256_hex_known_value() {
        let digest = sha256_hex(b"hello world");
        assert_eq!(
            digest,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_hex_empty() {
        let digest = sha256_hex(b"");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sanitized_token_trims_blank_values() {
        assert_eq!(sanitized_token(None), None);
        assert_eq!(sanitized_token(Some("")), None);
        assert_eq!(sanitized_token(Some("   ")), None);
        assert_eq!(sanitized_token(Some(" token\n")), Some("token"));
    }

    #[test]
    fn test_authorization_header_value_rejects_malformed_tokens() {
        assert!(authorization_header_value("token").is_some());
        assert!(authorization_header_value("bad\ntoken").is_none());
    }

    #[test]
    fn test_attestation_lookup_retries_auth_failures_without_token() {
        assert!(should_retry_attestations_without_token(
            StatusCode::UNAUTHORIZED,
            Some("token")
        ));
        assert!(should_retry_attestations_without_token(
            StatusCode::FORBIDDEN,
            Some("token")
        ));
        assert!(!should_retry_attestations_without_token(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("token")
        ));
        assert!(!should_retry_attestations_without_token(
            StatusCode::UNAUTHORIZED,
            Some("")
        ));
        assert!(!should_retry_attestations_without_token(
            StatusCode::UNAUTHORIZED,
            Some("bad\ntoken")
        ));
        assert!(!should_retry_attestations_without_token(
            StatusCode::UNAUTHORIZED,
            None
        ));
    }

    // -----------------------------------------------------------------------
    // Path validation and extraction hardening tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_entry_path_accepts_safe_paths() {
        assert!(validate_entry_path(Path::new("goose")).is_ok());
        assert!(validate_entry_path(Path::new("goose-package/goose")).is_ok());
        assert!(validate_entry_path(Path::new("subdir/nested/file.txt")).is_ok());
    }

    #[test]
    fn test_validate_entry_path_rejects_absolute() {
        let result = validate_entry_path(Path::new("/etc/malicious"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("absolute path"));
    }

    #[test]
    fn test_validate_entry_path_rejects_traversal() {
        let result = validate_entry_path(Path::new("../../escape.txt"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[test]
    fn test_validate_entry_path_rejects_nested_traversal() {
        let result = validate_entry_path(Path::new("safe/../../escape"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_extract_tar_bz2_safe_archive() {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let tmp = tempdir().unwrap();

        let mut builder_buf = Vec::new();
        {
            let encoder = BzEncoder::new(&mut builder_buf, Compression::default());
            let mut builder = tar::Builder::new(encoder);

            let data = b"goose binary content";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "goose-package/goose", &data[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        extract_tar_bz2(&builder_buf, tmp.path()).unwrap();

        let extracted = tmp.path().join("goose-package/goose");
        assert!(extracted.exists());
        assert_eq!(
            fs::read_to_string(extracted).unwrap(),
            "goose binary content"
        );
    }

    // -----------------------------------------------------------------------
    // Sigstore provenance verification test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_verify_provenance_fails_closed_when_unverifiable() {
        let result = verify_provenance(b"not a real archive", "stable").await;
        assert!(
            result.is_err(),
            "verify_provenance must fail closed when provenance cannot be verified"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_extract_tar_bz2_blocks_symlink_escape() {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;

        let tmp = tempdir().unwrap();

        let mut builder_buf = Vec::new();
        {
            let encoder = BzEncoder::new(&mut builder_buf, Compression::default());
            let mut builder = tar::Builder::new(encoder);

            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_cksum();
            // Symlink whose target escapes the destination directory.
            builder
                .append_link(&mut header, "evil_link", "../../etc/passwd")
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let result = extract_tar_bz2(&builder_buf, tmp.path());
        assert!(
            result.is_err(),
            "extraction should fail when a symlink target escapes the destination"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("link or special-file"),
            "error should identify the forbidden archive type, got: {err_msg}"
        );
    }
}
