//! Native Git inspection, worktree listing, diff parsing, and review operations.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("failed to execute git: {0}")]
    Io(#[from] std::io::Error),
    #[error("git command failed: {0}")]
    Command(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub staged: bool,
    pub kind: FileChangeKind,
}

#[derive(Debug, Clone, Default)]
pub struct RepoStatus {
    pub root: PathBuf,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLineKind {
    Header,
    Hunk,
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git").current_dir(cwd).args(args).output()?;
    if !out.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the git repository top-level root containing `start`.
pub fn repo_root(start: &Path) -> Result<PathBuf, GitError> {
    let raw = run_git(start, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(raw.trim()))
}

/// Read branch, ahead/behind counts, and staged/unstaged/untracked files.
pub fn repo_status(start: &Path) -> Result<RepoStatus, GitError> {
    let root = repo_root(start)?;
    let raw = run_git(&root, &["status", "--porcelain=v2", "--branch"])?;

    let mut status = RepoStatus {
        root,
        branch: "(detached)".into(),
        ahead: 0,
        behind: 0,
        files: Vec::new(),
    };

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            status.branch = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    status.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            status.files.push(ChangedFile {
                path: path.to_string(),
                staged: false,
                kind: FileChangeKind::Untracked,
            });
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let parts: Vec<&str> = line.splitn(9, ' ').collect();
            if parts.len() >= 9 {
                let xy = parts[1].as_bytes();
                let raw_path = parts[8];
                let path = if line.starts_with("2 ") {
                    raw_path.split('\t').next().unwrap_or(raw_path).to_string()
                } else {
                    raw_path.to_string()
                };
                if xy.len() == 2 {
                    if xy[0] != b'.' {
                        status.files.push(ChangedFile {
                            path: path.clone(),
                            staged: true,
                            kind: change_kind(xy[0]),
                        });
                    }
                    if xy[1] != b'.' {
                        status.files.push(ChangedFile {
                            path,
                            staged: false,
                            kind: change_kind(xy[1]),
                        });
                    }
                }
            }
        }
    }

    Ok(status)
}

fn change_kind(code: u8) -> FileChangeKind {
    match code {
        b'A' => FileChangeKind::Added,
        b'D' => FileChangeKind::Deleted,
        b'R' | b'C' => FileChangeKind::Renamed,
        _ => FileChangeKind::Modified,
    }
}

/// Compute parsed unified diff lines for a single path (supports untracked files cleanly).
pub fn file_diff(root: &Path, path: &str, staged: bool, untracked: bool) -> Result<Vec<DiffLine>, GitError> {
    if untracked {
        let full = root.join(path);
        let content = std::fs::read_to_string(&full).unwrap_or_default();
        let mut lines = vec![DiffLine {
            kind: DiffLineKind::Header,
            text: format!("+++ b/{path} (new untracked file)"),
        }];
        for l in content.lines().take(400) {
            lines.push(DiffLine {
                kind: DiffLineKind::Added,
                text: format!("+{l}"),
            });
        }
        return Ok(lines);
    }

    let raw = if staged {
        run_git(root, &["diff", "--cached", "--no-color", "--", path])?
    } else {
        run_git(root, &["diff", "--no-color", "--", path])?
    };

    Ok(parse_unified_diff(&raw))
}

pub fn parse_unified_diff(raw: &str) -> Vec<DiffLine> {
    raw.lines()
        .map(|line| {
            let kind = if line.starts_with("@@") {
                DiffLineKind::Hunk
            } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("index ") {
                DiffLineKind::Header
            } else if line.starts_with('+') {
                DiffLineKind::Added
            } else if line.starts_with('-') {
                DiffLineKind::Removed
            } else {
                DiffLineKind::Context
            };
            DiffLine {
                kind,
                text: line.to_string(),
            }
        })
        .collect()
}

pub fn stage_file(root: &Path, path: &str) -> Result<(), GitError> {
    let _ = run_git(root, &["add", "--", path])?;
    Ok(())
}

pub fn unstage_file(root: &Path, path: &str) -> Result<(), GitError> {
    // Works both before and after initial commit.
    if run_git(root, &["restore", "--staged", "--", path]).is_err() {
        let _ = run_git(root, &["rm", "--cached", "--", path])?;
    }
    Ok(())
}

pub fn list_worktrees(start: &Path) -> Result<Vec<WorktreeEntry>, GitError> {
    let root = repo_root(start)?;
    let raw = run_git(&root, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head = String::new();
    let mut cur_branch: Option<String> = None;

    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(prev) = cur_path.take() {
                entries.push(WorktreeEntry {
                    path: prev,
                    head: std::mem::take(&mut cur_head),
                    branch: cur_branch.take(),
                });
            }
            cur_path = Some(PathBuf::from(p.trim()));
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            cur_head = h.trim().to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(
                b.trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(b.trim())
                    .to_string(),
            );
        }
    }
    if let Some(prev) = cur_path {
        entries.push(WorktreeEntry {
            path: prev,
            head: cur_head,
            branch: cur_branch,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_stage_unstage_diff_and_worktree_on_temp_repo() {
        let tmp = std::env::temp_dir().join(format!(
            "hermes-git-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        run_git(&tmp, &["init", "-b", "main"]).unwrap();
        run_git(&tmp, &["config", "user.email", "test@hermes.local"]).unwrap();
        run_git(&tmp, &["config", "user.name", "Hermes Test"]).unwrap();

        std::fs::write(tmp.join("hello.txt"), "line1\n").unwrap();
        stage_file(&tmp, "hello.txt").unwrap();
        run_git(&tmp, &["commit", "-m", "init"]).unwrap();

        // Modify tracked file + add untracked file
        std::fs::write(tmp.join("hello.txt"), "line1\nline2\n").unwrap();
        std::fs::write(tmp.join("notes.md"), "# hi\n").unwrap();

        let st = repo_status(&tmp).unwrap();
        assert_eq!(st.branch, "main");
        assert!(st.files.iter().any(|f| f.path == "hello.txt" && !f.staged));
        assert!(st
            .files
            .iter()
            .any(|f| f.path == "notes.md" && f.kind == FileChangeKind::Untracked));

        let diff = file_diff(&st.root, "hello.txt", false, false).unwrap();
        assert!(diff
            .iter()
            .any(|l| l.kind == DiffLineKind::Added && l.text == "+line2"));

        stage_file(&st.root, "hello.txt").unwrap();
        let st2 = repo_status(&tmp).unwrap();
        assert!(st2.files.iter().any(|f| f.path == "hello.txt" && f.staged));

        unstage_file(&st.root, "hello.txt").unwrap();
        let wts = list_worktrees(&tmp).unwrap();
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
