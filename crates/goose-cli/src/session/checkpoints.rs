use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use goose::config::paths::Paths;
use goose::conversation::Conversation;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CHECKPOINT_VERSION: u32 = 1;
const MAX_CHECKPOINTS_PER_SESSION: usize = 100;
const MAX_CHECKPOINT_BYTES_PER_SESSION: u64 = 256 * 1024 * 1024;

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
    ) -> Result<TurnCheckpoint> {
        ensure_private_directory(&self.directory)?;
        let created_at = Utc::now();
        let id = self.available_id(created_at)?;
        let journal_root = self.directory.parent().map(Path::to_path_buf);
        let (code, code_unavailable_reason) =
            match capture_git(working_dir, authorized_root, journal_root.as_deref()) {
                Ok(snapshot) => (Some(snapshot), None),
                Err(error) => (None, Some(format!("{error:#}"))),
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
        self.write(&checkpoint)?;
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
        let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = fs::read_dir(&self.directory)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                Some((
                    entry.path(),
                    metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                    metadata.len(),
                ))
            })
            .collect();
        files.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
        let mut retained_bytes = 0_u64;
        for (index, (path, _, bytes)) in files.into_iter().enumerate() {
            // Always retain the newest checkpoint, even if one exceptionally
            // large conversation exceeds the journal's normal byte budget.
            let over_budget = index > 0
                && retained_bytes
                    .checked_add(bytes)
                    .is_none_or(|total| total > MAX_CHECKPOINT_BYTES_PER_SESSION);
            if index >= MAX_CHECKPOINTS_PER_SESSION || over_budget {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to prune checkpoint {}", path.display()))?;
            } else {
                retained_bytes += bytes;
            }
        }
        Ok(())
    }
}

pub(super) fn restore_git(
    checkpoint: &GitCheckpoint,
    working_dir: &Path,
    authorized_root: Option<&Path>,
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

    // Preflight both objects before changing either the worktree or index.
    git_success(
        &repo_root,
        [
            "cat-file",
            "-e",
            &format!("{}^{{tree}}", checkpoint.worktree_tree),
        ],
        None,
    )?;
    git_success(
        &repo_root,
        [
            "cat-file",
            "-e",
            &format!("{}^{{tree}}", checkpoint.index_tree),
        ],
        None,
    )?;

    // Represent the current worktree in a temporary index, then let Git's
    // unpack-trees machinery perform a checked, no-overlay transition. This
    // removes non-ignored paths created after the checkpoint without touching
    // ignored files or the user's real index.
    let temporary = tempfile::tempdir().context("Failed to create temporary checkpoint index")?;
    let index_path = temporary.path().join("index");
    populate_worktree_index(&repo_root, &index_path, &checkpoint.excluded_paths)?;
    git_success(
        &repo_root,
        ["read-tree", "--reset", "-u", &checkpoint.worktree_tree],
        Some(&index_path),
    )?;

    // Restore staged state independently after the filesystem matches.
    git_success(
        &repo_root,
        ["read-tree", "--reset", &checkpoint.index_tree],
        None,
    )?;
    Ok(())
}

fn capture_git(
    working_dir: &Path,
    authorized_root: Option<&Path>,
    excluded_path: Option<&Path>,
) -> Result<GitCheckpoint> {
    let repo_root = discover_repo_root(working_dir)?;
    enforce_authorized_root(&repo_root, authorized_root)?;
    reject_unsupported_repository(&repo_root)?;

    let head = git_output(&repo_root, ["rev-parse", "--verify", "HEAD"], None)
        .ok()
        .map(|bytes| String::from_utf8(bytes).context("Git HEAD is not UTF-8"))
        .transpose()?
        .map(|value| value.trim().to_string());
    let index_tree = String::from_utf8(git_output(&repo_root, ["write-tree"], None)?)
        .context("Git index tree ID is not UTF-8")?
        .trim()
        .to_string();

    let temporary = tempfile::tempdir().context("Failed to create temporary checkpoint index")?;
    let index_path = temporary.path().join("index");
    let excluded_paths = excluded_path
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&repo_root))
        .into_iter()
        .collect::<Vec<_>>();
    populate_worktree_index(&repo_root, &index_path, &excluded_paths)?;
    let worktree_tree =
        String::from_utf8(git_output(&repo_root, ["write-tree"], Some(&index_path))?)
            .context("Git worktree tree ID is not UTF-8")?
            .trim()
            .to_string();

    Ok(GitCheckpoint {
        repo_root,
        head,
        index_tree,
        worktree_tree,
        excluded_paths,
    })
}

fn populate_worktree_index(
    repo_root: &Path,
    index_path: &Path,
    excluded_paths: &[PathBuf],
) -> Result<()> {
    git_success(repo_root, ["read-tree", "--empty"], Some(index_path))?;
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
        let object = hash_blob(repo_root, &bytes)?;
        let cache_info = format!("{mode},{object},{relative}");
        git_success(
            repo_root,
            ["update-index", "--add", "--cacheinfo", &cache_info],
            Some(index_path),
        )?;
    }
    Ok(())
}

fn hash_blob(repo_root: &Path, bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["hash-object", "--no-filters", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start Git hash-object")?;
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
    let output = git_output(working_dir, ["rev-parse", "--show-toplevel"], None)
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
    let sparse = git_output(repo_root, ["config", "--bool", "core.sparseCheckout"], None)
        .ok()
        .is_some_and(|value| String::from_utf8_lossy(&value).trim() == "true");
    anyhow::ensure!(
        !sparse,
        "Code checkpoints do not support sparse checkouts; conversation rewind remains available"
    );

    let index = git_output(repo_root, ["ls-files", "--stage", "-z"], None)?;
    let has_submodule = index
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "));
    anyhow::ensure!(
        !has_submodule,
        "Code checkpoints do not capture submodule working trees; conversation rewind remains available"
    );
    Ok(())
}

fn git_success<I, S>(repo: &Path, args: I, index: Option<&Path>) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output(repo, args, index).map(|_| ())
}

fn git_output<I, S>(repo: &Path, args: I, index: Option<&Path>) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
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
        String::from_utf8(git_output(repo, args.iter().copied(), None).unwrap())
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
    fn code_restore_preserves_index_worktree_untracked_and_ignored_layers() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let journal = CheckpointJournal::at(state.path(), "session-2").unwrap();

        fs::write(repo.path().join("tracked.txt"), "staged\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        fs::write(repo.path().join("tracked.txt"), "worktree\n").unwrap();
        fs::write(repo.path().join("untracked.txt"), "untracked\n").unwrap();
        fs::write(repo.path().join("ignored.txt"), "ignored-before\n").unwrap();

        let checkpoint = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "change",
            )
            .unwrap();
        let code = checkpoint.code.as_ref().unwrap();

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        fs::remove_file(repo.path().join("untracked.txt")).unwrap();
        fs::write(repo.path().join("created-later.txt"), "remove me\n").unwrap();
        fs::write(repo.path().join("ignored.txt"), "ignored-after\n").unwrap();

        restore_git(code, repo.path(), Some(repo.path())).unwrap();

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
    fn code_checkpoint_refuses_repository_outside_authorized_root() {
        let repo = repository();
        let other = tempfile::tempdir().unwrap();
        let error = capture_git(repo.path(), Some(other.path()), None).unwrap_err();
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

        let snapshot = capture_git(repo.path(), Some(repo.path()), None).unwrap();
        assert!(
            !marker.exists(),
            "automatic capture executed a clean filter"
        );

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        restore_git(&snapshot, repo.path(), Some(repo.path())).unwrap();
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

        let snapshot = capture_git(repo.path(), Some(repo.path()), None).unwrap();
        fs::remove_file(&executable).unwrap();
        fs::remove_file(repo.path().join("run-link")).unwrap();
        fs::write(repo.path().join("run-link"), "not a link\n").unwrap();

        restore_git(&snapshot, repo.path(), Some(repo.path())).unwrap();
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
            )
            .unwrap();
        let second = journal
            .capture(
                repo.path(),
                Some(repo.path()),
                &Conversation::default(),
                "second",
            )
            .unwrap();
        let code = second.code.as_ref().unwrap();
        let tree = git(
            repo.path(),
            &["ls-tree", "-r", "--name-only", &code.worktree_tree],
        );
        assert!(!tree.contains(".goose-state"));

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        restore_git(code, repo.path(), Some(repo.path())).unwrap();
        assert_eq!(journal.list().unwrap().len(), 2);
    }

    #[test]
    fn invalid_session_ids_cannot_escape_state_directory() {
        assert!(CheckpointJournal::at(Path::new("/tmp"), "../escape").is_err());
        assert!(CheckpointJournal::at(Path::new("/tmp"), "a/b").is_err());
    }
}
