use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use goose::config::paths::Paths;
use goose::conversation::Conversation;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CHECKPOINT_VERSION: u32 = 1;
const MAX_CHECKPOINTS_PER_SESSION: usize = 100;
const MAX_CHECKPOINT_BYTES_PER_SESSION: u64 = 256 * 1024 * 1024;
const MAX_CODE_CHECKPOINT_CAPTURE_BYTES: u64 = 128 * 1024 * 1024;
const OBJECT_STORES_DIRECTORY: &str = "objects";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct TurnCheckpoint {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub prompt: String,
    pub working_dir: PathBuf,
    pub conversation: Conversation,
    pub code: Option<GitCheckpoint>,
    pub code_unavailable_reason: Option<String>,
    version: u32,
    session_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct GitCheckpoint {
    pub repo_root: PathBuf,
    pub head: Option<String>,
    pub index_tree: String,
    pub worktree_tree: String,
    excluded_paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object_store: Option<PathBuf>,
}

#[derive(Debug)]
pub(super) struct CheckpointJournal {
    session_id: String,
    directory: PathBuf,
}

impl CheckpointJournal {
    pub fn new(session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        Ok(Self {
            session_id: session_id.to_string(),
            directory: Paths::state_dir().join("turn-checkpoints").join(session_id),
        })
    }

    #[cfg(test)]
    fn at(root: &Path, session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        Ok(Self {
            session_id: session_id.to_string(),
            directory: root.join(session_id),
        })
    }

    /// Capture the conversation and, when the working directory is a supported
    /// Git worktree, its index and non-ignored worktree contents. A code
    /// snapshot failure never prevents the conversation checkpoint.
    pub fn capture(
        &self,
        working_dir: &Path,
        authorized_root: Option<&Path>,
        conversation: &Conversation,
        prompt: &str,
        code_capture_block_reason: Option<&str>,
    ) -> Result<TurnCheckpoint> {
        ensure_private_directory(&self.directory)?;
        let created_at = Utc::now();
        let id = self.available_id(created_at)?;
        let journal_root = self.directory.parent().map(Path::to_path_buf);
        let (code, code_unavailable_reason) = match code_capture_block_reason {
            Some(reason) => (None, Some(reason.to_string())),
            None => match capture_git(
                working_dir,
                authorized_root,
                journal_root.as_deref(),
                &self.directory,
                &id,
            ) {
                Ok(snapshot) => (Some(snapshot), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            },
        };
        let checkpoint = TurnCheckpoint {
            id,
            created_at,
            prompt: prompt.to_string(),
            working_dir: working_dir
                .canonicalize()
                .unwrap_or_else(|_| working_dir.to_path_buf()),
            conversation: conversation.clone(),
            code,
            code_unavailable_reason,
            version: CHECKPOINT_VERSION,
            session_id: self.session_id.clone(),
        };
        if let Err(error) = self.write(&checkpoint) {
            if let Some(code) = checkpoint.code.as_ref() {
                let _ = self.remove_object_store(code);
            }
            return Err(error);
        }
        self.prune()?;
        Ok(checkpoint)
    }

    pub fn list(&self) -> Result<Vec<TurnCheckpoint>> {
        let mut checkpoints = Vec::new();
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(checkpoints),
            Err(error) => return Err(error).context("Failed to read checkpoint journal"),
        };
        for entry in entries {
            let entry = entry?;
            if entry.path().extension() != Some(OsStr::new("json")) {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            let checkpoint: TurnCheckpoint = serde_json::from_slice(&bytes)
                .with_context(|| format!("Invalid checkpoint {}", entry.path().display()))?;
            anyhow::ensure!(
                checkpoint.version == CHECKPOINT_VERSION,
                "Unsupported checkpoint version {}",
                checkpoint.version
            );
            anyhow::ensure!(
                checkpoint.session_id == self.session_id,
                "Checkpoint belongs to a different session"
            );
            checkpoints.push(checkpoint);
        }
        checkpoints.sort_by_key(|checkpoint| std::cmp::Reverse(checkpoint.created_at));
        Ok(checkpoints)
    }

    pub fn get(&self, selector: &str) -> Result<TurnCheckpoint> {
        let selector = selector.trim();
        anyhow::ensure!(!selector.is_empty(), "Checkpoint ID is required");
        let matches: Vec<TurnCheckpoint> = self
            .list()?
            .into_iter()
            .filter(|checkpoint| checkpoint.id.starts_with(selector))
            .collect();
        match matches.as_slice() {
            [checkpoint] => Ok(checkpoint.clone()),
            [] => anyhow::bail!("No checkpoint matches '{selector}'"),
            _ => anyhow::bail!("Checkpoint prefix '{selector}' is ambiguous"),
        }
    }

    fn available_id(&self, created_at: DateTime<Utc>) -> Result<String> {
        let stem = created_at.format("%Y%m%dT%H%M%S%.3fZ").to_string();
        for suffix in 0..1_000_u16 {
            let id = if suffix == 0 {
                stem.clone()
            } else {
                format!("{stem}-{suffix}")
            };
            if !self.directory.join(format!("{id}.json")).exists() {
                return Ok(id);
            }
        }
        anyhow::bail!("Could not allocate a unique checkpoint ID")
    }

    fn write(&self, checkpoint: &TurnCheckpoint) -> Result<()> {
        let final_path = self.directory.join(format!("{}.json", checkpoint.id));
        let mut temporary = tempfile::Builder::new()
            .prefix(".checkpoint-")
            .tempfile_in(&self.directory)?;
        set_private_file(temporary.path())?;
        serde_json::to_writer(&mut temporary, checkpoint)?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist_noclobber(&final_path)
            .map_err(|error| error.error)
            .with_context(|| format!("Failed to save checkpoint {}", checkpoint.id))?;
        Ok(())
    }

    fn prune(&self) -> Result<()> {
        self.prune_with_limits(
            MAX_CHECKPOINTS_PER_SESSION,
            MAX_CHECKPOINT_BYTES_PER_SESSION,
        )
    }

    fn prune_with_limits(&self, max_checkpoints: usize, max_bytes: u64) -> Result<()> {
        let mut files: Vec<(PathBuf, std::time::SystemTime, u64, TurnCheckpoint)> =
            fs::read_dir(&self.directory)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
                .map(|entry| {
                    let metadata = entry.metadata()?;
                    let bytes = fs::read(entry.path())?;
                    let checkpoint: TurnCheckpoint =
                        serde_json::from_slice(&bytes).with_context(|| {
                            format!("Invalid checkpoint {}", entry.path().display())
                        })?;
                    let object_bytes = checkpoint
                        .code
                        .as_ref()
                        .map(|code| self.object_store_size(code))
                        .transpose()?
                        .unwrap_or(0);
                    Ok((
                        entry.path(),
                        metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                        metadata.len().saturating_add(object_bytes),
                        checkpoint,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
        files.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
        let mut retained_bytes = 0_u64;
        let mut retained_object_stores = HashSet::new();
        for (index, (path, _, bytes, checkpoint)) in files.into_iter().enumerate() {
            // Always retain the newest checkpoint, even if one exceptionally
            // large conversation exceeds the journal's normal byte budget.
            let over_budget = index > 0
                && retained_bytes
                    .checked_add(bytes)
                    .is_none_or(|total| total > max_bytes);
            if index >= max_checkpoints || over_budget {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to prune checkpoint {}", path.display()))?;
                if let Some(code) = checkpoint.code.as_ref() {
                    self.remove_object_store(code)?;
                }
            } else {
                retained_bytes += bytes;
                if let Some(relative) = checkpoint
                    .code
                    .as_ref()
                    .and_then(|code| code.object_store.as_ref())
                {
                    retained_object_stores.insert(relative.clone());
                }
            }
        }
        self.remove_orphaned_object_stores(&retained_object_stores)?;
        Ok(())
    }

    pub fn restore_git(
        &self,
        checkpoint: &GitCheckpoint,
        working_dir: &Path,
        authorized_root: Option<&Path>,
    ) -> Result<()> {
        let object_store = self.resolve_object_store(checkpoint)?;
        restore_git_with_object_store(
            checkpoint,
            working_dir,
            authorized_root,
            object_store.as_deref(),
        )
    }

    fn resolve_object_store(&self, checkpoint: &GitCheckpoint) -> Result<Option<PathBuf>> {
        let Some(relative) = checkpoint.object_store.as_deref() else {
            return Ok(None);
        };
        validate_object_store_path(relative)?;
        let path = self.directory.join(relative);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("Checkpoint object store {} is missing", path.display()))?;
        let journal = self
            .directory
            .canonicalize()
            .context("Checkpoint journal no longer exists")?;
        anyhow::ensure!(
            canonical.starts_with(&journal) && canonical.is_dir(),
            "Checkpoint object store escapes its private journal"
        );
        Ok(Some(canonical))
    }

    fn object_store_size(&self, checkpoint: &GitCheckpoint) -> Result<u64> {
        self.resolve_object_store(checkpoint)?
            .map(|path| directory_size(&path))
            .transpose()
            .map(|size| size.unwrap_or(0))
    }

    fn remove_object_store(&self, checkpoint: &GitCheckpoint) -> Result<()> {
        let Some(path) = self.resolve_object_store(checkpoint)? else {
            return Ok(());
        };
        fs::remove_dir_all(&path)
            .with_context(|| format!("Failed to prune object store {}", path.display()))
    }

    fn remove_orphaned_object_stores(&self, retained: &HashSet<PathBuf>) -> Result<()> {
        let root = self.directory.join(OBJECT_STORES_DIRECTORY);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("Failed to inspect checkpoint object stores"),
        };
        for entry in entries {
            let entry = entry?;
            let relative = PathBuf::from(OBJECT_STORES_DIRECTORY).join(entry.file_name());
            if retained.contains(&relative) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path()).with_context(|| {
                    format!(
                        "Failed to remove orphaned object store {}",
                        entry.path().display()
                    )
                })?;
            }
        }
        Ok(())
    }
}

fn restore_git_with_object_store(
    checkpoint: &GitCheckpoint,
    working_dir: &Path,
    authorized_root: Option<&Path>,
    object_store_path: Option<&Path>,
) -> Result<()> {
    let repo_root = discover_repo_root(working_dir)?;
    anyhow::ensure!(
        repo_root == checkpoint.repo_root,
        "Checkpoint belongs to {}, but the active repository is {}",
        checkpoint.repo_root.display(),
        repo_root.display()
    );
    enforce_authorized_root(&repo_root, authorized_root)?;
    reject_unsupported_repository(&repo_root)?;
    let object_store = object_store_path
        .map(|path| object_environment(&repo_root, path))
        .transpose()?;
    let transition_store =
        tempfile::tempdir().context("Failed to create temporary rewind object store")?;
    set_private_directory(transition_store.path())?;
    let mut transition_objects = object_environment(&repo_root, transition_store.path())?;
    if let Some(checkpoint_store) = object_store_path {
        transition_objects
            .alternates
            .insert(0, checkpoint_store.to_path_buf());
    }

    // Preflight both objects before changing either the worktree or index.
    git_success_with_objects(
        &repo_root,
        [
            "cat-file",
            "-e",
            &format!("{}^{{tree}}", checkpoint.worktree_tree),
        ],
        None,
        object_store.as_ref(),
    )?;
    git_success_with_objects(
        &repo_root,
        [
            "cat-file",
            "-e",
            &format!("{}^{{tree}}", checkpoint.index_tree),
        ],
        None,
        object_store.as_ref(),
    )?;

    // Represent the current worktree in a temporary index, then let Git's
    // unpack-trees machinery perform a checked, no-overlay transition. This
    // removes non-ignored paths created after the checkpoint without touching
    // ignored files or the user's real index.
    let temporary = tempfile::tempdir().context("Failed to create temporary checkpoint index")?;
    let index_path = temporary.path().join("index");
    populate_worktree_index(
        &repo_root,
        &index_path,
        &checkpoint.excluded_paths,
        Some(&transition_objects),
        None,
    )?;
    git_success_with_objects(
        &repo_root,
        ["read-tree", "--reset", "-u", &checkpoint.worktree_tree],
        Some(&index_path),
        Some(&transition_objects),
    )?;

    // Restore staged state independently after the filesystem matches.
    if let Some(object_store) = object_store.as_ref() {
        materialize_tree_blobs_in_repository(&repo_root, &checkpoint.index_tree, object_store)?;
    }
    git_success_with_objects(
        &repo_root,
        ["read-tree", "--reset", &checkpoint.index_tree],
        None,
        object_store.as_ref(),
    )?;
    Ok(())
}

fn capture_git(
    working_dir: &Path,
    authorized_root: Option<&Path>,
    excluded_path: Option<&Path>,
    checkpoint_directory: &Path,
    checkpoint_id: &str,
) -> Result<GitCheckpoint> {
    let repo_root = discover_repo_root(working_dir)?;
    enforce_authorized_root(&repo_root, authorized_root)?;
    reject_unsupported_repository(&repo_root)?;

    let stores = checkpoint_directory.join(OBJECT_STORES_DIRECTORY);
    ensure_private_directory(&stores)?;
    let temporary = tempfile::Builder::new()
        .prefix(".capture-")
        .tempdir_in(&stores)
        .context("Failed to create temporary checkpoint object store")?;
    set_private_directory(temporary.path())?;
    let object_store = object_environment(&repo_root, temporary.path())?;
    let mut budget = CaptureBudget::new(MAX_CODE_CHECKPOINT_CAPTURE_BYTES);

    let head = git_output(&repo_root, ["rev-parse", "--verify", "HEAD"], None, None)
        .ok()
        .map(|bytes| String::from_utf8(bytes).context("Git HEAD is not UTF-8"))
        .transpose()?
        .map(|value| value.trim().to_string());
    materialize_index_blobs(&repo_root, &object_store, &mut budget)?;
    let index_tree = String::from_utf8(git_output(
        &repo_root,
        ["write-tree"],
        None,
        Some(&object_store),
    )?)
    .context("Git index tree ID is not UTF-8")?
    .trim()
    .to_string();

    let temporary_index =
        tempfile::tempdir().context("Failed to create temporary checkpoint index")?;
    let index_path = temporary_index.path().join("index");
    let excluded_paths = excluded_path
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&repo_root))
        .into_iter()
        .collect::<Vec<_>>();
    populate_worktree_index(
        &repo_root,
        &index_path,
        &excluded_paths,
        Some(&object_store),
        Some(&mut budget),
    )?;
    let worktree_tree = String::from_utf8(git_output(
        &repo_root,
        ["write-tree"],
        Some(&index_path),
        Some(&object_store),
    )?)
    .context("Git worktree tree ID is not UTF-8")?
    .trim()
    .to_string();

    let relative_store = PathBuf::from(OBJECT_STORES_DIRECTORY).join(checkpoint_id);
    validate_object_store_path(&relative_store)?;
    let final_store = checkpoint_directory.join(&relative_store);
    anyhow::ensure!(
        !final_store.exists(),
        "Checkpoint object store already exists"
    );
    fs::rename(temporary.path(), &final_store)
        .context("Failed to publish checkpoint object store")?;

    Ok(GitCheckpoint {
        repo_root,
        head,
        index_tree,
        worktree_tree,
        excluded_paths,
        object_store: Some(relative_store),
    })
}

fn populate_worktree_index(
    repo_root: &Path,
    index_path: &Path,
    excluded_paths: &[PathBuf],
    object_store: Option<&ObjectEnvironment>,
    mut budget: Option<&mut CaptureBudget>,
) -> Result<()> {
    git_success_with_objects(
        repo_root,
        ["read-tree", "--empty"],
        Some(index_path),
        object_store,
    )?;
    let listed = git_output(
        repo_root,
        [
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        None,
        None,
    )?;
    for raw_path in listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = String::from_utf8(raw_path.to_vec())
            .context("Code checkpoints do not support non-UTF-8 repository paths")?;
        validate_git_path(&relative)?;
        let absolute = repo_root.join(&relative);
        if excluded_paths
            .iter()
            .any(|excluded| absolute.starts_with(excluded))
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("Cannot inspect {relative}")),
        };
        let (mode, bytes) = if metadata.file_type().is_symlink() {
            let target = fs::read_link(&absolute)?;
            let target = target
                .to_str()
                .context("Code checkpoints do not support non-UTF-8 symlink targets")?;
            ("120000", target.as_bytes().to_vec())
        } else if metadata.is_file() {
            if let Some(budget) = budget.as_deref() {
                budget.ensure_individual_object(metadata.len())?;
            }
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            (
                if executable { "100755" } else { "100644" },
                fs::read(&absolute).with_context(|| format!("Cannot read {relative}"))?,
            )
        } else {
            anyhow::bail!(
                "Code checkpoints do not support special file or directory entry '{relative}'"
            );
        };
        let object = if let Some(object_store) = object_store {
            hash_blob(repo_root, &bytes, Some(&object_store.primary))?
        } else {
            hash_blob(repo_root, &bytes, None)?
        };
        if let Some(budget) = budget.as_deref_mut() {
            budget.record(&object, bytes.len() as u64)?;
        }
        let cache_info = format!("{mode},{object},{relative}");
        git_success_with_objects(
            repo_root,
            ["update-index", "--add", "--cacheinfo", &cache_info],
            Some(index_path),
            object_store,
        )?;
    }
    Ok(())
}

fn hash_blob(repo_root: &Path, bytes: &[u8], object_store: Option<&Path>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo_root)
        .args(["hash-object", "--no-filters", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(object_store) = object_store {
        command
            .env("GIT_OBJECT_DIRECTORY", object_store)
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    }
    let mut child = command.spawn().context("Failed to start Git hash-object")?;
    child
        .stdin
        .take()
        .context("Git hash-object stdin was unavailable")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    Ok(String::from_utf8(check_git_output(output)?)
        .context("Git object ID is not UTF-8")?
        .trim()
        .to_string())
}

#[derive(Debug)]
struct ObjectEnvironment {
    primary: PathBuf,
    alternates: Vec<PathBuf>,
}

#[derive(Debug)]
struct CaptureBudget {
    limit: u64,
    bytes: u64,
    objects: HashSet<String>,
}

impl CaptureBudget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            bytes: 0,
            objects: HashSet::new(),
        }
    }

    fn record(&mut self, object: &str, bytes: u64) -> Result<()> {
        if !self.objects.insert(object.to_string()) {
            return Ok(());
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("Code checkpoint capture size overflowed")?;
        anyhow::ensure!(
            self.bytes <= self.limit,
            "Code checkpoint exceeds the {} MiB capture limit; conversation rewind remains available",
            self.limit / (1024 * 1024)
        );
        Ok(())
    }

    fn ensure_individual_object(&self, bytes: u64) -> Result<()> {
        anyhow::ensure!(
            bytes <= self.limit,
            "Code checkpoint exceeds the {} MiB capture limit; conversation rewind remains available",
            self.limit / (1024 * 1024)
        );
        Ok(())
    }
}

fn materialize_index_blobs(
    repo_root: &Path,
    object_store: &ObjectEnvironment,
    budget: &mut CaptureBudget,
) -> Result<()> {
    let listed = git_output(
        repo_root,
        ["ls-files", "--stage", "-z"],
        None,
        Some(object_store),
    )?;
    for entry in listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let metadata = entry
            .split(|byte| *byte == b'\t')
            .next()
            .unwrap_or_default();
        let fields = metadata
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        anyhow::ensure!(fields.len() == 3, "Git returned an invalid index entry");
        anyhow::ensure!(
            fields[2] == b"0",
            "Code checkpoints do not support conflicted indexes"
        );
        let object = std::str::from_utf8(fields[1]).context("Git object ID is not UTF-8")?;
        if budget.objects.contains(object) {
            continue;
        }
        let size = String::from_utf8(git_output(
            repo_root,
            ["cat-file", "-s", object],
            None,
            Some(object_store),
        )?)
        .context("Git object size is not UTF-8")?
        .trim()
        .parse::<u64>()
        .context("Git object size is invalid")?;
        budget.ensure_individual_object(size)?;
        let bytes = git_output(
            repo_root,
            ["cat-file", "blob", object],
            None,
            Some(object_store),
        )?;
        let copied = hash_blob(repo_root, &bytes, Some(&object_store.primary))?;
        anyhow::ensure!(copied == object, "Git object changed while checkpointing");
        budget.record(object, bytes.len() as u64)?;
    }
    Ok(())
}

fn materialize_tree_blobs_in_repository(
    repo_root: &Path,
    tree: &str,
    object_store: &ObjectEnvironment,
) -> Result<()> {
    let listed = git_output(
        repo_root,
        ["ls-tree", "-r", "-z", tree],
        None,
        Some(object_store),
    )?;
    for entry in listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let metadata = entry
            .split(|byte| *byte == b'\t')
            .next()
            .unwrap_or_default();
        let fields = metadata
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        anyhow::ensure!(fields.len() == 3, "Git returned an invalid tree entry");
        if fields[1] != b"blob" {
            continue;
        }
        let object = std::str::from_utf8(fields[2]).context("Git object ID is not UTF-8")?;
        let bytes = git_output(
            repo_root,
            ["cat-file", "blob", object],
            None,
            Some(object_store),
        )?;
        let copied = hash_blob(repo_root, &bytes, None)?;
        anyhow::ensure!(
            copied == object,
            "Git object changed while restoring checkpoint"
        );
    }
    Ok(())
}

fn object_environment(repo_root: &Path, primary: &Path) -> Result<ObjectEnvironment> {
    let raw = String::from_utf8(git_output(
        repo_root,
        ["rev-parse", "--git-path", "objects"],
        None,
        None,
    )?)
    .context("Git object directory is not UTF-8")?;
    let alternate = PathBuf::from(raw.trim());
    let alternate = if alternate.is_absolute() {
        alternate
    } else {
        repo_root.join(alternate)
    }
    .canonicalize()
    .context("Git object directory does not exist")?;
    let primary = primary
        .canonicalize()
        .context("Checkpoint object directory does not exist")?;
    Ok(ObjectEnvironment {
        primary,
        alternates: vec![alternate],
    })
}

fn validate_object_store_path(relative: &Path) -> Result<()> {
    anyhow::ensure!(
        !relative.is_absolute()
            && relative.components().count() == 2
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && relative.starts_with(OBJECT_STORES_DIRECTORY),
        "Invalid checkpoint object store path"
    );
    Ok(())
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "Checkpoint object store contains a symlink"
            );
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .context("Checkpoint object store size overflowed")?;
            }
        }
    }
    Ok(total)
}

fn validate_git_path(relative: &str) -> Result<()> {
    let path = Path::new(relative);
    anyhow::ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|component| { matches!(component, Component::Normal(_) | Component::CurDir) }),
        "Git returned an unsafe repository path"
    );
    Ok(())
}

fn discover_repo_root(working_dir: &Path) -> Result<PathBuf> {
    let output = git_output(working_dir, ["rev-parse", "--show-toplevel"], None, None)
        .context("The active directory is not inside a Git repository")?;
    let raw = String::from_utf8(output).context("Git repository path is not UTF-8")?;
    PathBuf::from(raw.trim())
        .canonicalize()
        .context("Git repository root no longer exists")
}

fn enforce_authorized_root(repo_root: &Path, authorized_root: Option<&Path>) -> Result<()> {
    if let Some(authorized_root) = authorized_root {
        let authorized_root = authorized_root
            .canonicalize()
            .context("The authorized workspace root no longer exists")?;
        anyhow::ensure!(
            repo_root.starts_with(&authorized_root),
            "Repository {} is outside the authorized workspace {}",
            repo_root.display(),
            authorized_root.display()
        );
    }
    Ok(())
}

fn reject_unsupported_repository(repo_root: &Path) -> Result<()> {
    let sparse = git_output(
        repo_root,
        ["config", "--bool", "core.sparseCheckout"],
        None,
        None,
    )
    .ok()
    .is_some_and(|value| String::from_utf8_lossy(&value).trim() == "true");
    anyhow::ensure!(
        !sparse,
        "Code checkpoints do not support sparse checkouts; conversation rewind remains available"
    );

    let index = git_output(repo_root, ["ls-files", "--stage", "-z"], None, None)?;
    let has_submodule = index
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "));
    anyhow::ensure!(
        !has_submodule,
        "Code checkpoints do not capture submodule working trees; conversation rewind remains available"
    );
    Ok(())
}

fn git_success_with_objects<I, S>(
    repo: &Path,
    args: I,
    index: Option<&Path>,
    objects: Option<&ObjectEnvironment>,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output(repo, args, index, objects).map(|_| ())
}

fn git_output<I, S>(
    repo: &Path,
    args: I,
    index: Option<&Path>,
    objects: Option<&ObjectEnvironment>,
) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    if let Some(objects) = objects {
        let alternates = std::env::join_paths(&objects.alternates)
            .context("Git object directory cannot be represented as an alternate")?;
        command
            .env("GIT_OBJECT_DIRECTORY", &objects.primary)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates);
    }
    let output = command.output().context("Failed to start Git")?;
    check_git_output(output)
}

fn check_git_output(output: Output) -> Result<Vec<u8>> {
    anyhow::ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn validate_session_id(session_id: &str) -> Result<()> {
    anyhow::ensure!(
        !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "Invalid session ID for checkpoint journal"
    );
    anyhow::ensure!(
        Path::new(session_id)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "Invalid session ID for checkpoint journal"
    );
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("Failed to create checkpoint directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use goose::conversation::message::Message;

    fn git(repo: &Path, args: &[&str]) -> String {
        String::from_utf8(git_output(repo, args.iter().copied(), None, None).unwrap())
            .unwrap()
            .trim()
            .to_string()
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init"]);
        git(
            directory.path(),
            &["config", "user.name", "Checkpoint Test"],
        );
        git(
            directory.path(),
            &["config", "user.email", "checkpoint@example.invalid"],
        );
        fs::write(directory.path().join("tracked.txt"), "original\n").unwrap();
        fs::write(directory.path().join(".gitignore"), "ignored.txt\n").unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-m", "initial"]);
        directory
    }

    #[test]
    fn journal_round_trips_conversation_and_reports_non_git_code() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-1").unwrap();
        let conversation = Conversation::new_unvalidated([Message::user().with_text("previous")]);

        let saved = journal
            .capture(
                workspace.path(),
                Some(workspace.path()),
                &conversation,
                "next",
                None,
            )
            .unwrap();
        assert!(saved.code.is_none());
        assert!(saved
            .code_unavailable_reason
            .as_deref()
            .unwrap()
            .contains("not inside a Git repository"));

        let loaded = journal.get(saved.id.get(..8).unwrap()).unwrap();
        assert_eq!(loaded.prompt, "next");
        assert_eq!(loaded.conversation, conversation);
        assert_eq!(journal.list().unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&journal.directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(journal.directory.join(format!("{}.json", saved.id)))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn conversation_only_capture_does_not_write_git_objects() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-read-only").unwrap();
        fs::write(repo.path().join("untracked.txt"), "not a git object\n").unwrap();
        let objects_before = git(repo.path(), &["count-objects", "-v"]);

        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "inspect only",
                Some("Code capture is unavailable under the read-only capability"),
            )
            .unwrap();

        assert!(checkpoint.code.is_none());
        assert_eq!(
            checkpoint.code_unavailable_reason.as_deref(),
            Some("Code capture is unavailable under the read-only capability")
        );
        assert_eq!(git(repo.path(), &["count-objects", "-v"]), objects_before);
        assert_eq!(journal.list().unwrap().len(), 1);
    }

    #[test]
    fn code_restore_preserves_index_worktree_untracked_and_ignored_layers() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-2").unwrap();

        fs::write(repo.path().join("tracked.txt"), "staged\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        fs::write(repo.path().join("tracked.txt"), "worktree\n").unwrap();
        fs::write(repo.path().join("untracked.txt"), "untracked\n").unwrap();
        fs::write(repo.path().join("ignored.txt"), "ignored-before\n").unwrap();
        let repository_objects_before = git(repo.path(), &["count-objects", "-v"]);

        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "change",
                None,
            )
            .unwrap();
        let code = checkpoint.code.as_ref().unwrap();
        let object_store = journal.resolve_object_store(code).unwrap().unwrap();
        assert!(object_store.starts_with(&journal.directory));
        assert_ne!(directory_size(&object_store).unwrap(), 0);
        assert_eq!(
            git(repo.path(), &["count-objects", "-v"]),
            repository_objects_before,
            "checkpoint capture wrote into the repository object database"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&object_store).unwrap().permissions().mode() & 0o077,
                0,
                "checkpoint object store is not private"
            );
        }

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        fs::write(repo.path().join("tracked.txt"), "later worktree\n").unwrap();
        fs::remove_file(repo.path().join("untracked.txt")).unwrap();
        fs::write(repo.path().join("created-later.txt"), "remove me\n").unwrap();
        fs::write(repo.path().join("ignored.txt"), "ignored-after\n").unwrap();
        git(repo.path(), &["gc", "--prune=now"]);

        journal
            .restore_git(code, repo.path(), Some(repo.path()))
            .unwrap();

        assert_eq!(
            fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "worktree\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("untracked.txt")).unwrap(),
            "untracked\n"
        );
        assert!(!repo.path().join("created-later.txt").exists());
        assert_eq!(
            fs::read_to_string(repo.path().join("ignored.txt")).unwrap(),
            "ignored-after\n"
        );
        assert_eq!(git(repo.path(), &["show", ":tracked.txt"]), "staged");
        assert!(git(repo.path(), &["status", "--short"])
            .lines()
            .any(|line| line == "?? untracked.txt"));
    }

    #[test]
    fn pruning_removes_checkpoint_object_stores_and_counts_their_bytes() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-prune").unwrap();

        fs::write(repo.path().join("untracked.txt"), "first payload\n").unwrap();
        let first = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "first",
                None,
            )
            .unwrap();
        let first_store = journal
            .resolve_object_store(first.code.as_ref().unwrap())
            .unwrap()
            .unwrap();

        fs::write(repo.path().join("untracked.txt"), "second payload\n").unwrap();
        let second = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "second",
                None,
            )
            .unwrap();
        let second_store = journal
            .resolve_object_store(second.code.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let second_bytes = directory_size(&second_store).unwrap();
        let second_json_bytes = fs::metadata(journal.directory.join(format!("{}.json", second.id)))
            .unwrap()
            .len();

        journal
            .prune_with_limits(100, second_bytes.saturating_add(second_json_bytes))
            .unwrap();

        let retained = journal.list().unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].id, second.id);
        assert!(!first_store.exists());
        assert!(second_store.exists());
    }

    #[test]
    fn oversized_code_capture_falls_back_to_conversation_without_leaking_objects() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-oversized").unwrap();
        let oversized = fs::File::create(repo.path().join("oversized.bin")).unwrap();
        oversized
            .set_len(MAX_CODE_CHECKPOINT_CAPTURE_BYTES + 1)
            .unwrap();
        let repository_objects_before = git(repo.path(), &["count-objects", "-v"]);

        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "large repository",
                None,
            )
            .unwrap();

        assert!(checkpoint.code.is_none());
        assert!(checkpoint
            .code_unavailable_reason
            .as_deref()
            .unwrap()
            .contains("128 MiB capture limit"));
        assert_eq!(
            git(repo.path(), &["count-objects", "-v"]),
            repository_objects_before
        );
        let stores = journal.directory.join(OBJECT_STORES_DIRECTORY);
        assert_eq!(fs::read_dir(stores).unwrap().count(), 0);
        assert_eq!(journal.list().unwrap().len(), 1);
    }

    #[test]
    fn code_checkpoint_refuses_repository_outside_authorized_root() {
        let repo = repository();
        let other = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let error = capture_git(
            repo.path(),
            Some(other.path()),
            None,
            state.path(),
            "outside-root",
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the authorized workspace"));
    }

    #[cfg(unix)]
    #[test]
    fn automatic_capture_does_not_execute_repository_clean_filters() {
        let repo = repository();
        let marker = repo.path().join("filter-ran");
        let filter = repo.path().join("clean-filter.sh");
        fs::write(
            &filter,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&filter, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(
            repo.path().join(".gitattributes"),
            "*.txt filter=adversarial\n",
        )
        .unwrap();
        git(
            repo.path(),
            &[
                "config",
                "filter.adversarial.clean",
                filter.to_str().unwrap(),
            ],
        );
        fs::write(repo.path().join("tracked.txt"), "raw checkpoint bytes\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "filter-test").unwrap();
        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "filter",
                None,
            )
            .unwrap();
        let snapshot = checkpoint.code.as_ref().unwrap();
        assert!(
            !marker.exists(),
            "automatic capture executed a clean filter"
        );

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        journal
            .restore_git(snapshot, repo.path(), Some(repo.path()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "raw checkpoint bytes\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn code_restore_preserves_executable_and_symlink_modes() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let repo = repository();
        let executable = repo.path().join("run.sh");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("run.sh", repo.path().join("run-link")).unwrap();

        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "mode-test").unwrap();
        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "modes",
                None,
            )
            .unwrap();
        let snapshot = checkpoint.code.as_ref().unwrap();
        fs::remove_file(&executable).unwrap();
        fs::remove_file(repo.path().join("run-link")).unwrap();
        fs::write(repo.path().join("run-link"), "not a link\n").unwrap();

        journal
            .restore_git(snapshot, repo.path(), Some(repo.path()))
            .unwrap();
        assert_ne!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(
            fs::read_link(repo.path().join("run-link")).unwrap(),
            PathBuf::from("run.sh")
        );
    }

    #[test]
    fn checkpoint_storage_inside_repo_is_never_snapshotted_or_deleted() {
        let repo = repository();
        let journal =
            CheckpointJournal::at(&repo.path().join(".goose-state"), "session-3").unwrap();
        journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "first",
                None,
            )
            .unwrap();
        let second = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "second",
                None,
            )
            .unwrap();
        let code = second.code.as_ref().unwrap();
        let objects = journal.resolve_object_store(code).unwrap().unwrap();
        let object_environment = object_environment(repo.path(), &objects).unwrap();
        let tree = String::from_utf8(
            git_output(
                repo.path(),
                ["ls-tree", "-r", "--name-only", &code.worktree_tree],
                None,
                Some(&object_environment),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!tree.contains(".goose-state"));

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        journal
            .restore_git(code, repo.path(), Some(repo.path()))
            .unwrap();
        assert_eq!(journal.list().unwrap().len(), 2);
    }

    #[test]
    fn invalid_session_ids_cannot_escape_state_directory() {
        assert!(CheckpointJournal::at(Path::new("/tmp"), "../escape").is_err());
        assert!(CheckpointJournal::at(Path::new("/tmp"), "a/b").is_err());
    }
}
