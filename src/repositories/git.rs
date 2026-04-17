//! Slender git CLI service: command fabricators, write-sequence plumbing, &
//! the per-repository write lock that holds concurrent index mutations in check.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use uuid::Uuid;

use super::errors::{internal, ApiError};

/// Fabricate a `git -C <repo_dir>` command ready for invocation.
fn git_in(repo_dir: &str) -> Command {
    let mut c = Command::new("git");
    c.args(["-C", repo_dir]);
    c
}

/// Execute `git -C <repo_dir> <args>` & yield stdout bytes.
///
/// Inability to *spawn* git invariably surfaces the same uniform message (`Failed to run git command`).
/// A non-zero exit discharges the caller-supplied `on_fail` error - ensuring each handler
/// retains its specific 4xx/5xx payload intact.
pub fn run_git(repo_dir: &str, args: &[&str], on_fail: ApiError) -> Result<Vec<u8>, ApiError> {
    let result = git_in(repo_dir).args(args).output();
    match result {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(o) => {
            eprintln!("[repos] git {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
            Err(on_fail)
        }
        Err(e) => {
            eprintln!("[repos] Failed to run git: {e}");
            Err(internal("Failed to run git command"))
        }
    }
}

/// Expedient wrapper that surfaces stdout as a UTF-8 lossy string.
pub fn run_git_str(repo_dir: &str, args: &[&str], on_fail: ApiError) -> Result<String, ApiError> {
    run_git(repo_dir, args, on_fail).map(|b| String::from_utf8_lossy(&b).to_string())
}

/// Per-repository write lock - enlisted to serialise plumbing-based file writes &
/// ensure the bare repository's index file is never besieged by two concurrent writers
/// simultaneously.
fn write_lock(repo: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(repo.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Yields true if `path` is present within the given branch's tree.
pub fn path_exists(repo_dir: &str, branch: &str, path: &str) -> bool {
    let spec = format!("{branch}:{path}");
    Command::new("git")
        .args(["-C", repo_dir, "cat-file", "-e", &spec])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub enum WriteUpdate {
    Add { path: String, content: Vec<u8> },
    Remove { path: String },
}

/// Execute a solitary git plumbing step - mapping both spawn-failures &
/// non-zero exits onto the same error label (consonant with the erstwhile
/// behaviour of the inline write sequence).
fn run_step(repo_dir: &str, args: &[&str], label: &'static str) -> Result<Vec<u8>, ApiError> {
    let result = git_in(repo_dir).args(args).output();
    match result {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(o) => {
            eprintln!("[repos] {label}: {}", String::from_utf8_lossy(&o.stderr));
            Err(internal(label))
        }
        Err(e) => {
            eprintln!("[repos] {label} error: {e}");
            Err(internal(label))
        }
    }
}

/// Apply a solitary mutation (add or removal) to a branch & return the freshly
/// minted commit SHA. Atomically serialised against concurrent writers on the same repo.
pub fn run_write_sequence(
    repo_dir: &str,
    branch: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
    update: WriteUpdate,
) -> Result<String, ApiError> {
    let lock = write_lock(repo_dir);
    let _guard = lock.lock().unwrap();

    // 1. Locate the antecedent commit on the branch (if one exists).
    let branch_ref = format!("refs/heads/{branch}");
    let parent_out = git_in(repo_dir)
        .args(["rev-parse", "--verify", &branch_ref])
        .output()
        .map_err(|e| {
            eprintln!("[repos] rev-parse error: {e}");
            internal("rev-parse failed")
        })?;
    let parent_sha = parent_out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&parent_out.stdout).trim().to_string());

    // 2. Prime the index from the parent tree (or empty for nascent branches).
    match parent_sha.as_deref() {
        Some(p) => run_step(repo_dir, &["read-tree", p], "read-tree failed")?,
        None => run_step(repo_dir, &["read-tree", "--empty"], "read-tree failed")?,
    };

    // 3. Imprint the requested mutation upon the index.
    match update {
        WriteUpdate::Add { path, content } => stage_add(repo_dir, &path, &content)?,
        WriteUpdate::Remove { path } => stage_remove(repo_dir, &path)?,
    }

    // 4. Materialise the index - tree.
    let tree_sha = String::from_utf8_lossy(&run_step(repo_dir, &["write-tree"], "write-tree failed")?)
        .trim()
        .to_string();

    // 5. Commit the tree, weaving author/committer identity through env vars.
    let mut commit = git_in(repo_dir);
    commit.args(["commit-tree", &tree_sha]);
    if let Some(p) = parent_sha.as_deref() {
        commit.args(["-p", p]);
    }
    let commit_out = commit
        .args(["-m", message])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", author_name)
        .env("GIT_COMMITTER_EMAIL", author_email)
        .output()
        .map_err(|e| {
            eprintln!("[repos] commit-tree error: {e}");
            internal("commit-tree failed")
        })?;
    if !commit_out.status.success() {
        eprintln!("[repos] commit-tree: {}", String::from_utf8_lossy(&commit_out.stderr));
        return Err(internal("commit-tree failed"));
    }
    let new_sha = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();

    // 6. Propel the branch ref forward.
    run_step(
        repo_dir,
        &["update-ref", &branch_ref, &new_sha],
        "update-ref failed",
    )?;

    Ok(new_sha)
}

fn stage_add(repo_dir: &str, path: &str, content: &[u8]) -> Result<(), ApiError> {
    let mut hash = git_in(repo_dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!("[repos] hash-object spawn failed: {e}");
            internal("hash-object failed")
        })?;
    hash.stdin
        .as_mut()
        .unwrap()
        .write_all(content)
        .map_err(|e| {
            eprintln!("[repos] hash-object write failed: {e}");
            internal("hash-object write failed")
        })?;
    let out = hash.wait_with_output().map_err(|e| {
        eprintln!("[repos] hash-object wait failed: {e}");
        internal("hash-object wait failed")
    })?;
    if !out.status.success() {
        return Err(internal("hash-object failed"));
    }
    let blob_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let cacheinfo = format!("100644,{blob_sha},{path}");
    run_step(
        repo_dir,
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
        "update-index failed",
    )?;
    Ok(())
}

fn stage_remove(repo_dir: &str, path: &str) -> Result<(), ApiError> {
    // `update-index --force-remove` demands a non-bare repo. Aim
    // GIT_WORK_TREE at a disposable temp directory so the operation concludes
    // without ever encroaching upon the (bare) repo's directory.
    let work_tree = std::env::temp_dir().join(format!("git-agent-wt-{}", Uuid::new_v4().simple()));
    let _ = std::fs::create_dir_all(&work_tree);
    let result = git_in(repo_dir)
        .args(["-c", "core.bare=false", "update-index", "--force-remove", path])
        .env("GIT_WORK_TREE", &work_tree)
        .output();
    let _ = std::fs::remove_dir_all(&work_tree);
    let out = result.map_err(|e| {
        eprintln!("[repos] update-index --remove error: {e}");
        internal("update-index failed")
    })?;
    if !out.status.success() {
        eprintln!(
            "[repos] update-index --remove: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(internal("update-index failed"));
    }
    Ok(())
}
