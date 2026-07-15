use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

fn git(root: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(root) = root {
        cmd.current_dir(root);
    }

    let output = cmd
        .args(args)
        .output()
        .context("failed to spawn git — is it installed?")?;

    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8(output.stdout)?)
}

pub fn repo_root() -> Result<PathBuf> {
    let out = git(None, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Resolve the base ref to a commit SHA. With `merge_base`, resolves to
/// `merge-base(<base>, HEAD)` — the fork point, mirroring what a PR diff
/// shows — rather than the ref's tip.
pub fn resolve_base(root: &Path, base: &str, merge_base: bool) -> Result<String> {
    let sha = if merge_base {
        git(Some(root), &["merge-base", base, "HEAD"])?
    } else {
        git(Some(root), &["rev-parse", base])?
    };
    Ok(sha.trim().to_owned())
}

/// Relative paths of all files under `dir_rel` at the given commit. Includes
/// fixtures deleted in the working tree so they can surface as Removed.
pub fn list_files_at(root: &Path, sha: &str, dir_rel: &str) -> Result<Vec<String>> {
    let out = git(
        Some(root),
        &["ls-tree", "-r", "--name-only", sha, "--", dir_rel],
    )?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Contents of `path_rel` at the given commit, or `None` if the file did not
/// exist there (i.e. the fixture is new on this branch).
///
/// `--filters` applies checkout filters — the snapshots are git-lfs tracked,
/// so a plain `git show` would return the LFS pointer, not the payload.
pub fn read_file_at(root: &Path, sha: &str, path_rel: &str) -> Result<Option<String>> {
    match git(
        Some(root),
        &["cat-file", "--filters", &format!("{sha}:{path_rel}")],
    ) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.to_string().contains("does not exist") => Ok(None),
        Err(e) if e.to_string().contains("could not get object info") => Ok(None),
        Err(e) => Err(e),
    }
}
