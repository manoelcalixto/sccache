// Copyright 2026 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Local Git worktree discovery without invoking the `git` executable.

use crate::errors::*;
use fs_err as fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitWorktreeContext {
    root: PathBuf,
    common_dir: PathBuf,
}

impl GitWorktreeContext {
    pub(crate) fn discover(cwd: &Path) -> Result<Option<Self>> {
        for candidate in cwd.ancestors() {
            let dot_git = candidate.join(".git");
            if dot_git.is_dir() {
                return Self::from_git_dir(candidate, &dot_git).map(Some);
            }
            if dot_git.is_file() {
                let git_dir = read_gitdir_file(&dot_git)?;
                let git_dir = if git_dir.is_absolute() {
                    git_dir
                } else {
                    candidate.join(git_dir)
                };
                return Self::from_git_dir(candidate, &git_dir).map(Some);
            }
        }
        Ok(None)
    }

    fn from_git_dir(root: &Path, git_dir: &Path) -> Result<Self> {
        let git_dir = fs::canonicalize(git_dir)
            .with_context(|| format!("Failed to resolve Git directory {}", git_dir.display()))?;
        let common_dir_file = git_dir.join("commondir");
        let common_dir = if common_dir_file.is_file() {
            let path = read_path_file(&common_dir_file, "commondir")?;
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        } else {
            git_dir
        };

        Ok(Self {
            // Keep the spelling Cargo uses for paths. Canonicalization introduces
            // platform-specific prefixes such as /private on macOS and \\?\ on
            // Windows, preventing lexical worktree-relative path matching.
            root: root.to_owned(),
            common_dir: fs::canonicalize(&common_dir).with_context(|| {
                format!(
                    "Failed to resolve Git common directory {}",
                    common_dir.display()
                )
            })?,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    pub(crate) fn relative_path<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.root).ok()
    }
}

fn read_gitdir_file(path: &Path) -> Result<PathBuf> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read Git worktree file {}", path.display()))?;
    let git_dir = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Malformed Git worktree file {}", path.display()))?;
    Ok(PathBuf::from(git_dir))
}

fn read_path_file(path: &Path, kind: &str) -> Result<PathBuf> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read Git {kind} file {}", path.display()))?;
    let value = contents.trim();
    if value.is_empty() {
        bail!("Empty Git {kind} file {}", path.display());
    }
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_main_worktree_from_nested_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        let nested = root.join("crates/example");
        fs::create_dir_all(root.join(".git"))?;
        fs::create_dir_all(&nested)?;

        let context = GitWorktreeContext::discover(&nested)?.expect("Git context");
        assert_eq!(context.root(), root);
        assert_eq!(context.common_dir(), fs::canonicalize(root.join(".git"))?);
        assert_eq!(
            context.relative_path(&nested),
            Some(Path::new("crates/example"))
        );
        Ok(())
    }

    #[test]
    fn linked_worktrees_share_the_common_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        let common = main.join(".git");
        let linked_git_dir = common.join("worktrees/linked");
        fs::create_dir_all(&linked_git_dir)?;
        fs::create_dir_all(&linked)?;
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", linked_git_dir.display()),
        )?;
        fs::write(linked_git_dir.join("commondir"), "../..\n")?;

        let context = GitWorktreeContext::discover(&linked)?.expect("Git context");
        assert_eq!(context.root(), linked);
        assert_eq!(context.common_dir(), fs::canonicalize(&common)?);
        Ok(())
    }

    #[test]
    fn supports_relative_gitdir_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("linked");
        let git_dir = temp.path().join("metadata/worktrees/linked");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&git_dir)?;
        fs::write(root.join(".git"), "gitdir: ../metadata/worktrees/linked\n")?;

        let context = GitWorktreeContext::discover(&root)?.expect("Git context");
        assert_eq!(context.common_dir(), fs::canonicalize(&git_dir)?);
        Ok(())
    }

    #[test]
    fn returns_none_outside_git() -> Result<()> {
        let temp = tempfile::tempdir()?;
        assert_eq!(GitWorktreeContext::discover(temp.path())?, None);
        Ok(())
    }

    #[test]
    fn rejects_malformed_gitdir_file() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join(".git"), "not a gitdir\n")?;
        assert!(GitWorktreeContext::discover(temp.path()).is_err());
        Ok(())
    }
}
