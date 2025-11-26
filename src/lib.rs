use anyhow::{Result, bail};
use cmd_lib::run_fun;
use remote::{ListRepos, Remote};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{debug, info, warn};

pub mod remote;
#[cfg(feature = "tui")]
pub mod tui;

/// Represents a pattern that matches one or more repositories. It has the
/// format: [provider]/<path>.
#[derive(Debug, Eq, PartialEq)]
pub struct RepoPattern {
    /// The provider (e.g., "github.com", "gitlab.com")
    pub provider: Option<String>,

    /// The repo path
    pub path: String,
}

impl FromStr for RepoPattern {
    type Err = Box<dyn Error>;

    fn from_str(path: &str) -> std::prelude::v1::Result<Self, Self::Err> {
        // Split on first '/' to separate provider from path
        let parts: Vec<&str> = path.splitn(2, '/').collect();

        if parts.len() == 2 {
            // Has a '/', so first part might be provider
            let first = parts[0];
            let rest = parts[1];

            // If first part looks like a domain (contains '.'), it's a provider
            if first.contains('.') {
                Ok(Self {
                    provider: Some(first.to_string()),
                    path: rest.to_string(),
                })
            } else {
                // Otherwise, the whole thing is the path
                Ok(Self {
                    provider: None,
                    path: path.to_string(),
                })
            }
        } else {
            // No '/', so just a simple path
            Ok(Self {
                provider: None,
                path: path.to_string(),
            })
        }
    }
}

impl RepoPattern {
    /// Get the provider and path as a tuple if provider exists
    pub fn provider_and_path(&self) -> Option<(&str, &str)> {
        self.provider
            .as_ref()
            .map(|p| (p.as_str(), self.path.as_str()))
    }

    /// Get the full path including provider if it exists
    pub fn full_path(&self) -> String {
        match &self.provider {
            Some(provider) => format!("{}/{}", provider, self.path),
            None => self.path.clone(),
        }
    }

    /// Check if this pattern matches a given path
    /// Supports exact matches and partial matches
    pub fn matches(&self, test_path: &str) -> bool {
        let full = self.full_path();

        // Exact match
        if test_path == full {
            return true;
        }

        // Ends with match (for simple repo names)
        if test_path.ends_with(&full) {
            return true;
        }

        // Contains match for path component
        if test_path.contains(&self.path) {
            return true;
        }

        false
    }
}

/// Recursively find "top-level" git repositories.
/// This function will not traverse into .git directories or nested git repositories.
/// Find all git repositories in the given path
pub fn find_git_repositories(path: &str) -> Result<Vec<PathBuf>> {
    debug!(path = %path, "Recursively searching for git repositories");
    let mut found: Vec<PathBuf> = Vec::new();
    let path_buf = PathBuf::from(path);

    // Check if this path itself is a git repository
    if path_buf.join(".git").exists() {
        found.push(path_buf);
        return Ok(found); // Don't traverse into git repositories
    }

    // Otherwise, recursively search subdirectories
    if let Ok(entries) = std::fs::read_dir(&path_buf) {
        for entry in entries.filter_map(|e| e.ok()) {
            let entry_path = entry.path();

            // Skip .git directory itself (it's not a repo container)
            if let Some(name) = entry_path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str == ".git" {
                    continue;
                }
            }

            // Only traverse directories
            if entry_path.is_dir() {
                match find_git_repositories(&entry_path.to_string_lossy()) {
                    Ok(mut repos) => found.append(&mut repos),
                    Err(e) => {
                        // Log but don't fail on permission errors
                        debug!("Skipping {}: {}", entry_path.display(), e);
                    }
                }
            }
        }
    }

    Ok(found)
}

/// Check if a repository has uncommitted changes using git-oxide
/// This includes:
/// - Modified tracked files
/// - Deleted tracked files
/// - Staged changes
/// - Untracked files
pub fn check_uncommitted_changes(repo_path: &Path) -> Result<bool> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Failed to open repository at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(false); // If we can't open it, assume it's not dirty
        }
    };

    // Use gix's built-in is_dirty() method which checks:
    // - index vs working tree changes
    // - working tree vs index changes
    // - submodule status (respecting their ignore config)
    // Note: untracked files do NOT affect is_dirty(), so we check separately
    let is_dirty = match repo.is_dirty() {
        Ok(dirty) => dirty,
        Err(e) => {
            warn!(
                "Failed to check if repository is dirty at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(false);
        }
    };

    if is_dirty {
        debug!(
            "Repository has uncommitted changes: {}",
            repo_path.display()
        );
        return Ok(true);
    }

    // Also check for untracked files
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "Failed to create status platform at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(false);
        }
    };

    match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(mut iter) => {
            // Check if there are any untracked files
            for entry in iter.by_ref().flatten() {
                // Check if this is an untracked file
                if matches!(
                    entry,
                    gix::status::index_worktree::iter::Item::DirectoryContents { .. }
                ) {
                    debug!("Repository has untracked files: {}", repo_path.display());
                    return Ok(true);
                }
            }
        }
        Err(e) => {
            warn!(
                "Failed to check for untracked files at {}: {}",
                repo_path.display(),
                e
            );
        }
    }

    Ok(false)
}

/// Repository status information
#[derive(Debug, Default)]
pub struct RepoStatus {
    pub has_changes: bool,
    pub has_unpushed: bool,
    pub has_commits: bool,
}

/// Check repository status (commits, changes, unpushed) in a single pass
pub fn check_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    let mut status = RepoStatus::default();

    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Failed to open repository at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(status);
        }
    };

    // Check if repo has commits
    match repo.head() {
        Ok(head) => {
            match head.try_into_referent() {
                Some(head_ref) => {
                    status.has_commits = true;

                    // Check for unpushed commits
                    let local_branch = head_ref.name();
                    let remote_ref_name = match repo
                        .branch_remote_ref_name(local_branch, gix::remote::Direction::Fetch)
                    {
                        Some(Ok(name)) => name,
                        Some(Err(e)) => {
                            debug!("Failed to get remote ref: {}", e);
                            return Ok(status); // Can't check unpushed, but we have commits
                        }
                        None => {
                            debug!("No upstream branch configured");
                            return Ok(status); // No upstream, can't check unpushed
                        }
                    };

                    // Try to find the remote ref
                    match repo.find_reference(remote_ref_name.as_ref()) {
                        Ok(remote_ref) => {
                            let local_commit = match head_ref.id().object() {
                                Ok(obj) => obj.id,
                                Err(e) => {
                                    warn!("Failed to get local commit: {}", e);
                                    return Ok(status);
                                }
                            };

                            let remote_commit = match remote_ref.id().object() {
                                Ok(obj) => obj.id,
                                Err(e) => {
                                    warn!("Failed to get remote commit: {}", e);
                                    return Ok(status);
                                }
                            };

                            // If commits are different, we might have unpushed commits
                            if local_commit != remote_commit {
                                status.has_unpushed = true;
                            }
                        }
                        Err(_) => {
                            debug!("Remote ref not found, assuming no unpushed commits");
                        }
                    }
                }
                None => {
                    status.has_commits = false;
                }
            }
        }
        Err(_) => {
            status.has_commits = false;
        }
    }

    // Check for uncommitted changes
    match repo.is_dirty() {
        Ok(dirty) if dirty => {
            status.has_changes = true;
            return Ok(status);
        }
        Err(e) => {
            warn!(
                "Failed to check if repository is dirty at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(status);
        }
        _ => {}
    }

    // Also check for untracked files
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "Failed to create status platform at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(status);
        }
    };

    match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(mut iter) => {
            for entry in iter.by_ref().flatten() {
                if matches!(
                    entry,
                    gix::status::index_worktree::iter::Item::DirectoryContents { .. }
                ) {
                    status.has_changes = true;
                    break;
                }
            }
        }
        Err(e) => {
            warn!(
                "Failed to check for untracked files at {}: {}",
                repo_path.display(),
                e
            );
        }
    }

    Ok(status)
}

/// Check if a repository has any commits
pub fn check_has_commits(repo_path: &Path) -> Result<bool> {
    Ok(check_repo_status(repo_path)?.has_commits)
}

/// Check if a repository has unpushed commits
pub fn check_unpushed_commits(repo_path: &Path) -> Result<bool> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Failed to open repository at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(false);
        }
    };

    // Get the current branch
    let head = repo.head()?;
    let head_ref = match head.try_into_referent() {
        Some(r) => r,
        None => {
            debug!("HEAD is detached, no tracking branch");
            return Ok(false); // Detached HEAD, no upstream to compare
        }
    };

    // Get the upstream branch if it exists
    let local_branch = head_ref.name();
    let remote_ref_name =
        match repo.branch_remote_ref_name(local_branch, gix::remote::Direction::Fetch) {
            Some(Ok(name)) => name,
            Some(Err(e)) => {
                debug!("Error getting remote ref name: {}", e);
                return Ok(false);
            }
            None => {
                debug!("No upstream branch configured");
                return Ok(false); // No upstream configured
            }
        };

    // Try to find the remote reference
    let remote_ref = match repo.find_reference(remote_ref_name.as_ref()) {
        Ok(r) => r,
        Err(_) => {
            debug!("Remote reference not found: {:?}", remote_ref_name);
            return Ok(false); // Remote ref doesn't exist (never pushed)
        }
    };

    // Compare local and remote commit IDs
    let local_commit = head_ref.id();
    let remote_commit = remote_ref.id();

    Ok(local_commit != remote_commit)
}

/// A `Workspace` is filesystem directory containing git repositories checked out
/// from one or more providers. Each repository's path matches the remote's path,
/// for example:
///     <workspace path>/github.com/fossable/workset
///
/// This is stored in .workset.toml in the workspace root.
#[derive(Debug, Serialize, Deserialize)]
pub struct Workspace {
    /// The workspace directory's filesystem path (not serialized, set at runtime)
    #[serde(skip)]
    pub path: String,

    /// A list of providers for the workspace
    #[serde(default)]
    pub remotes: Vec<Remote>,

    /// The library directory for this workspace
    #[serde(flatten)]
    pub library: Option<Library>,
}

impl Default for Workspace {
    fn default() -> Self {
        let home = home::home_dir().expect("the home directory exists");

        Self {
            path: std::env::current_dir()
                .ok()
                .unwrap_or_else(|| home.join("workspace"))
                .display()
                .to_string(),
            remotes: vec![],
            library: Some(Library {
                path: home.join(".workset").display().to_string(),
            }),
        }
    }
}

impl Workspace {
    /// Load workspace from current directory.
    pub fn load() -> Result<Option<Self>> {
        let mut workspace_root = std::env::current_dir()?;

        // Search up for a workspace config
        let config_path = loop {
            if workspace_root.join(".workset.toml").exists() {
                break workspace_root.join(".workset.toml");
            }

            // Try parent directory
            match workspace_root.parent() {
                Some(parent) => workspace_root = parent.to_path_buf(),
                None => return Ok(None),
            }
        };

        debug!(config_path = %config_path.display(), "Loading workspace configuration");

        let config_content = std::fs::read_to_string(&config_path)
            .map_err(|e| anyhow::anyhow!("Failed to read workspace config: {}", e))?;

        let mut workspace: Workspace = toml::from_str(&config_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse workspace config: {}", e))?;

        // Set the workspace path to the actual root directory
        workspace.path = workspace_root.display().to_string();

        // Validate the workspace configuration
        workspace.validate()?;

        debug!(workspace = ?workspace, "Loaded workspace configuration");

        // Make sure library directory exists
        if let Some(library) = workspace.library.as_ref() {
            std::fs::create_dir_all(&library.path)
                .map_err(|e| anyhow::anyhow!("Failed to create library directory: {}", e))?;
        }

        Ok(Some(workspace))
    }

    /// Validate the workspace configuration
    fn validate(&self) -> Result<()> {
        // Check if workspace path exists
        if !Path::new(&self.path).exists() {
            bail!("Workspace path does not exist: {}", self.path);
        }

        // Validate library path if set
        if let Some(library) = &self.library {
            // Check if library path is absolute or relative
            let lib_path = Path::new(&library.path);
            if lib_path.to_string_lossy().contains("~") {
                warn!(
                    "Library path contains '~' which may not expand correctly: {}",
                    library.path
                );
            }
        }

        // Validate remote configurations
        for (idx, remote) in self.remotes.iter().enumerate() {
            if let Err(e) = remote.list_repo_paths() {
                warn!("Remote #{} validation failed: {}", idx + 1, e);
            }
        }

        Ok(())
    }

    /// Search the workspace for local repos matching the given pattern.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        let repos = find_git_repositories(&format!("{}/{}", self.path, pattern.full_path()))?;
        Ok(repos)
    }

    /// Clone/open a repository in this workspace
    pub fn open(&self, library: Option<&Library>, pattern: &RepoPattern) -> Result<PathBuf> {
        debug!(pattern = ?pattern, "Opening repos");

        // First check if repository already exists locally
        let local_repos = self.search(pattern)?;

        if !local_repos.is_empty() {
            let repo = &local_repos[0];
            info!("✓ Repository already in workspace: {}", repo.display());

            // Check if there are any uncommitted changes or unpushed commits
            if check_uncommitted_changes(repo)? {
                info!("  ⚠ Has uncommitted changes");
            }
            if check_unpushed_commits(repo)? {
                info!("  ⚠ Has unpushed commits");
            }

            return Ok(local_repos[0].clone());
        }

        // Check library and restore if found
        if let Some(library) = library {
            let relative_path = pattern.full_path();
            let repo_path = format!("{}/{}", self.path, relative_path);

            if library.exists(relative_path.clone()) {
                info!("📦 Restoring from library: {}", relative_path);
                library.restore_to_workspace(&self.path, &relative_path)?;

                // Fetch latest changes from upstream
                info!("  🔄 Fetching latest changes...");
                if let Err(e) = self.fetch_updates(&PathBuf::from(&repo_path)) {
                    debug!("Failed to fetch updates: {}", e);
                    info!("  ⚠ Could not fetch updates (continuing anyway)");
                }

                info!("✓ Restored {}", relative_path);
                return Ok(PathBuf::from(repo_path));
            }
        }

        // Try to clone from remotes
        info!("🔄 Cloning {}...", pattern.full_path());
        let repo_path = self.clone_from_remote(pattern)?;

        info!("✓ Successfully cloned to: {}", repo_path.display());
        Ok(repo_path)
    }

    /// Fetch updates for a repository
    fn fetch_updates(&self, _repo_path: &Path) -> Result<()> {
        // TODO: Implement fetch using gix once the API is clearer
        // For now, repositories are restored as-is from the library
        Ok(())
    }

    /// Drop a repository from this workspace
    pub fn drop(
        &self,
        library: Option<&Library>,
        pattern: &RepoPattern,
        delete: bool,
        force: bool,
    ) -> Result<()> {
        use tracing::{debug, info, warn};

        debug!("Drop requested for pattern: {:?}", pattern);

        let repos = self.search(pattern)?;

        if repos.is_empty() {
            warn!(
                "No repositories found matching pattern: {}",
                pattern.full_path()
            );
            return Ok(());
        }

        for repo in repos {
            // Check for uncommitted changes unless --force is given
            if !force {
                if check_uncommitted_changes(&repo)? {
                    warn!(
                        "⚠ Refusing to drop repository with uncommitted changes: {}",
                        repo.display()
                    );
                    warn!("  Use --force to drop anyway");
                    continue;
                }

                // Check for unpushed commits
                if check_unpushed_commits(&repo)? {
                    warn!(
                        "⚠ Refusing to drop repository with unpushed commits: {}",
                        repo.display()
                    );
                    warn!("  Use --force to drop anyway");
                    continue;
                }
            }

            if !delete {
                if let Some(library) = library {
                    // Store the repository in the library using workspace-relative path
                    info!("📦 Storing {} in library", repo.display());
                    let relative_path = repo
                        .strip_prefix(&self.path)
                        .unwrap_or(&repo)
                        .to_string_lossy()
                        .trim_start_matches('/')
                        .to_string();
                    library.store_from_workspace(&self.path, &relative_path)?;
                }
            } else {
                info!("🗑️  Permanently deleting {}", repo.display());
            }

            // Remove the directory
            debug!("Removing directory: {:?}", &repo);
            std::fs::remove_dir_all(&repo)?;
            info!("✓ Dropped {}", repo.display());
        }
        Ok(())
    }

    /// Drop all repositories in the current directory
    pub fn drop_all(&self, library: Option<&Library>, delete: bool, force: bool) -> Result<()> {
        use tracing::{debug, info, warn};

        debug!("Drop all requested in current directory");

        let cwd = std::env::current_dir()?;
        let repos = find_git_repositories(&cwd.to_string_lossy())?;

        if repos.is_empty() {
            info!("No repositories found in current directory");
            return Ok(());
        }

        info!("Found {} repository(ies) in current directory", repos.len());

        let mut dropped = 0;
        let mut skipped = 0;

        for repo in repos {
            // Check for uncommitted changes unless --force is given
            if !force {
                if check_uncommitted_changes(&repo)? {
                    warn!(
                        "⚠ Skipping repository with uncommitted changes: {}",
                        repo.display()
                    );
                    skipped += 1;
                    continue;
                }

                // Check for unpushed commits
                if check_unpushed_commits(&repo)? {
                    warn!(
                        "⚠ Skipping repository with unpushed commits: {}",
                        repo.display()
                    );
                    skipped += 1;
                    continue;
                }
            }

            if !delete {
                if let Some(library) = library {
                    // Store the repository in the library using workspace-relative path
                    info!("📦 Storing {} in library", repo.display());
                    let relative_path = repo
                        .strip_prefix(&self.path)
                        .unwrap_or(&repo)
                        .to_string_lossy()
                        .trim_start_matches('/')
                        .to_string();
                    library.store_from_workspace(&self.path, &relative_path)?;
                }
            } else {
                info!("🗑️  Permanently deleting {}", repo.display());
            }

            // Remove the directory
            debug!("Removing directory: {:?}", &repo);
            std::fs::remove_dir_all(&repo)?;
            info!("✓ Dropped {}", repo.display());
            dropped += 1;
        }

        if dropped > 0 {
            info!("✓ Dropped {} repository(ies)", dropped);
        }
        if skipped > 0 {
            warn!(
                "⚠ Skipped {} repository(ies) - use --force to drop anyway",
                skipped
            );
        }

        Ok(())
    }

    /// Attempt to clone a repository from configured remotes or infer the clone URL
    fn clone_from_remote(&self, pattern: &RepoPattern) -> Result<PathBuf> {
        use tracing::info;

        let dest_path = format!("{}/{}", self.path, pattern.full_path());

        // Try to infer the git URL from the pattern
        // Pattern could be:
        // - github.com/user/repo (with provider)
        // - user/repo (without provider, check configured remotes)

        if let Some((provider, repo_path)) = pattern.provider_and_path() {
            // Has provider like github.com/user/repo
            let clone_url = format!("https://{}/{}", provider, repo_path);

            std::fs::create_dir_all(std::path::Path::new(&dest_path).parent().unwrap())?;

            info!("Cloning {} to {}", clone_url, dest_path);
            run_fun!(git clone $clone_url $dest_path)?;

            return Ok(PathBuf::from(dest_path));
        }

        // No provider specified, would need to check configured remotes
        bail!("No provider specified. Use full path like github.com/user/repo")
    }
}

/// Stores repositories that are dropped from a `Workspace` in a library directory.
/// Entries in the library are bare repositories for space efficiency.
#[derive(Debug, Serialize, Deserialize)]
pub struct Library {
    pub path: String,
}

impl Library {
    /// Move the given repository into the library.
    /// workspace_path: the root path of the workspace
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn store_from_workspace(&self, workspace_path: &str, relative_path: &str) -> Result<()> {
        use tracing::debug;

        // Make sure the library directory exists first
        std::fs::create_dir_all(&self.path).map_err(|e| {
            anyhow::anyhow!("Failed to create library directory {}: {}", self.path, e)
        })?;

        let source = format!("{}/{}/.git", workspace_path, relative_path);
        let dest = format!("{}/{}", self.path, relative_path);

        // Verify the source .git directory exists
        if std::fs::metadata(&source).is_err() {
            bail!("Repository .git directory not found: {}", source);
        }

        run_fun!(git -C $source config core.bare true)
            .map_err(|e| anyhow::anyhow!("Failed to configure repository as bare: {}", e))?;

        debug!(source = %source, dest = %dest, "Storing repository in library");

        // Create parent directories in library if needed
        if let Some(parent) = Path::new(&dest).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create library parent directory: {}", e))?;
        }

        // Clear the library entry if it exists (for re-storing)
        if std::fs::metadata(&dest).is_ok() {
            debug!("Removing existing library entry: {}", dest);
            std::fs::remove_dir_all(&dest)
                .map_err(|e| anyhow::anyhow!("Failed to remove existing library entry: {}", e))?;
        }

        run_fun!(mv $source $dest)
            .map_err(|e| anyhow::anyhow!("Failed to move repository to library: {}", e))?;

        Ok(())
    }

    /// Restore a repository from the library to the workspace.
    /// workspace_path: the root path of the workspace
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn restore_to_workspace(&self, workspace_path: &str, relative_path: &str) -> Result<()> {
        let source = format!("{}/{}", self.path, relative_path);
        let dest = format!("{}/{}", workspace_path, relative_path);

        // Verify the library entry exists
        if std::fs::metadata(&source).is_err() {
            bail!(
                "Repository not found in library for path: {}",
                relative_path
            );
        }

        // Create parent directory if needed
        if let Some(parent) = Path::new(&dest).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create parent directory: {}", e))?;
        }

        run_fun!(git clone $source $dest)
            .map_err(|e| anyhow::anyhow!("Failed to restore repository from library: {}", e))?;

        Ok(())
    }

    pub fn exists(&self, repo_path: String) -> bool {
        std::fs::metadata(format!("{}/{}", self.path, repo_path)).is_ok()
    }

    /// List all repositories in the library
    pub fn list(&self) -> Result<Vec<String>> {
        use tracing::debug;

        if !Path::new(&self.path).exists() {
            return Ok(Vec::new());
        }

        let mut repos = Vec::new();

        // Recursively find all git repositories in the library
        fn find_repos(base_path: &str, current_path: &Path, repos: &mut Vec<String>) -> Result<()> {
            if current_path.is_dir() {
                // Check if this is a bare git repository
                if gix::open(current_path).is_ok() {
                    // Get the relative path from the library base
                    if let Ok(rel_path) = current_path.strip_prefix(base_path) {
                        let repo_path = rel_path.to_string_lossy().to_string();
                        if !repo_path.is_empty() {
                            repos.push(repo_path);
                        }
                    }
                    return Ok(()); // Don't recurse into git repos
                }

                // Recursively search subdirectories
                if let Ok(entries) = std::fs::read_dir(current_path) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        find_repos(base_path, &path, repos)?;
                    }
                }
            }
            Ok(())
        }

        find_repos(&self.path, Path::new(&self.path), &mut repos)?;

        debug!("Found {} repositories in library", repos.len());
        repos.sort();
        Ok(repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_workspace_default() {
        let workspace = Workspace::default();
        assert!(workspace.library.is_some());
        assert!(!workspace.path.is_empty());
    }

    #[test]
    fn test_library_exists() {
        let temp_dir = TempDir::new().unwrap();
        let library = Library {
            path: temp_dir.path().to_string_lossy().to_string(),
        };

        let repo_path = "/test/repo";
        assert!(!library.exists(repo_path.to_string()));

        // Create the library directory using the normal path
        let library_path = format!("{}{}", library.path, repo_path);
        fs::create_dir_all(&library_path).unwrap();

        assert!(library.exists(repo_path.to_string()));
    }

    #[test]
    fn test_find_git_repositories() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        // Create a git directory structure
        let repo1 = base_path.join("repo1");
        fs::create_dir_all(repo1.join(".git")).unwrap();

        let repo2 = base_path.join("nested/repo2");
        fs::create_dir_all(repo2.join(".git")).unwrap();

        let not_repo = base_path.join("not_a_repo");
        fs::create_dir_all(&not_repo).unwrap();

        let repos = find_git_repositories(&base_path.to_string_lossy()).unwrap();

        assert_eq!(repos.len(), 2);
        assert!(repos.iter().any(|p| p.ends_with("repo1")));
        assert!(repos.iter().any(|p| p.ends_with("repo2")));
    }

    #[test]
    fn test_parse_with_provider() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("github.com/user/repo")?;
        assert_eq!(pattern.provider, Some("github.com".to_string()));
        assert_eq!(pattern.path, "user/repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_without_provider() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("user/repo")?;
        assert_eq!(pattern.provider, None);
        assert_eq!(pattern.path, "user/repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_simple_path() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("repo")?;
        assert_eq!(pattern.provider, None);
        assert_eq!(pattern.path, "repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_gitlab_path() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("gitlab.com/company/project/repo")?;
        assert_eq!(pattern.provider, Some("gitlab.com".to_string()));
        assert_eq!(pattern.path, "company/project/repo".to_string());
        Ok(())
    }

    #[test]
    fn test_provider_and_path() {
        let pattern = RepoPattern {
            provider: Some("github.com".to_string()),
            path: "user/repo".to_string(),
        };
        let (provider, path) = pattern.provider_and_path().unwrap();
        assert_eq!(provider, "github.com");
        assert_eq!(path, "user/repo");
    }

    #[test]
    fn test_provider_and_path_none() {
        let pattern = RepoPattern {
            provider: None,
            path: "user/repo".to_string(),
        };
        assert!(pattern.provider_and_path().is_none());
    }

    #[test]
    fn test_full_path_with_provider() {
        let pattern = RepoPattern {
            provider: Some("github.com".to_string()),
            path: "user/repo".to_string(),
        };
        assert_eq!(pattern.full_path(), "github.com/user/repo");
    }

    #[test]
    fn test_full_path_without_provider() {
        let pattern = RepoPattern {
            provider: None,
            path: "user/repo".to_string(),
        };
        assert_eq!(pattern.full_path(), "user/repo");
    }
}
