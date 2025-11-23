use anyhow::{bail, Result};
use cmd_lib::run_fun;
use remote::Remote;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::error::Error;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::debug;

pub mod remote;
#[cfg(feature = "tui")]
pub mod tui;

/// Represents a pattern that matches one or more repositories. It has the
/// format: [provider]/[path].
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
        self.provider.as_ref().map(|p| (p.as_str(), self.path.as_str()))
    }

    /// Get the full path including provider if it exists
    pub fn full_path(&self) -> String {
        match &self.provider {
            Some(provider) => format!("{}/{}", provider, self.path),
            None => self.path.clone(),
        }
    }
}

/// Represents a workspace name passed via CLI
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorkspaceName(pub String);

impl WorkspaceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WorkspaceName {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        if s.is_empty() {
            bail!("Workspace name cannot be empty")
        }
        if s.contains(':') || s.contains('/') {
            bail!("Workspace name cannot contain ':' or '/'")
        }
        Ok(WorkspaceName(s))
    }
}

impl TryFrom<&str> for WorkspaceName {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self> {
        s.to_string().try_into()
    }
}


/// Recursively find "top-level" git repositories.
fn find_git_dir(path: &str) -> Result<Vec<PathBuf>> {
    debug!(path = %path, "Recursively searching for git repositories");
    let mut found: Vec<PathBuf> = Vec::new();

    match std::fs::metadata(format!("{}/.git", &path)) {
        Ok(_) => found.push(PathBuf::from(path)),
        Err(_) => {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();

                if std::fs::metadata(&path)?.is_dir() {
                    found.append(&mut find_git_dir(&path.to_string_lossy())?);
                }
            }
        }
    }

    Ok(found)
}

/// Check if a repository has uncommitted changes using git-oxide
fn has_uncommitted_changes(repo_path: &Path) -> Result<bool> {
    use tracing::warn;

    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to open repository at {}: {}", repo_path.display(), e);
            return Ok(false); // If we can't open it, assume it's not dirty
        }
    };

    // Get working directory
    let workdir = repo.work_dir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no working directory"))?;

    // Check for any changes using a simple approach:
    // 1. Check if HEAD exists
    let mut head = repo.head()?;
    let _head_commit = head.peel_to_commit_in_place()?;

    // 2. Check if index has changes (check entry count or state)
    let index = repo.index_or_empty()?;

    // For tracked files, check if they've been modified or deleted
    for entry in index.entries() {
        let file_path = workdir.join(gix::path::from_bstr(entry.path(&index)));

        // Check if file was deleted
        if !file_path.exists() {
            return Ok(true);
        }

        // Check if file size changed (quick check for modifications)
        if let Ok(metadata) = std::fs::metadata(&file_path) {
            // Size mismatch indicates changes
            if metadata.len() != entry.stat.size as u64 {
                return Ok(true);
            }

            // Check modification time as a hint (not definitive but fast)
            if let Ok(mtime) = metadata.modified() {
                if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    let file_mtime = duration.as_secs() as u32;
                    if file_mtime != entry.stat.mtime.secs {
                        // File might be modified, this is a heuristic
                        return Ok(true);
                    }
                }
            }
        }
    }

    Ok(false)
}

/// A `Workspace` is filesystem directory containing git repositories checked out
/// from one or more remotes. Each repository's path matches the remote's path,
/// for example:
///     <workspace path>/github.com/fossable/workset
///
/// This is stored in .workset.toml in the workspace root.
#[derive(Debug, Serialize, Deserialize)]
pub struct Workspace {
    /// A user-friendly name for the workspace like "personal" or "work"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

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
            name: None,
            path: std::env::current_dir()
                .ok()
                .unwrap_or_else(|| home.join("workspace"))
                .display()
                .to_string(),
            remotes: vec![],
            library: Some(Library {
                path: home.join(".local/share/workset/library").display().to_string(),
            }),
        }
    }
}

impl Workspace {
    /// Find the workspace root by searching for .workset.toml starting from current directory
    /// and walking up the directory tree.
    fn find_workspace_root() -> Result<PathBuf> {
        let mut current = std::env::current_dir()?;

        loop {
            let config_path = current.join(".workset.toml");
            if config_path.exists() {
                return Ok(current);
            }

            // Try parent directory
            match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => bail!("Not in a workset workspace. Run 'workset init' to create one."),
            }
        }
    }

    /// Load the workspace config from .workset.toml in the workspace root.
    pub fn load() -> Result<Self> {
        let workspace_root = Self::find_workspace_root()?;
        let config_path = workspace_root.join(".workset.toml");

        debug!(config_path = %config_path.display(), "Loading workspace configuration");

        let mut workspace: Workspace = toml::from_str(&std::fs::read_to_string(&config_path)?)?;

        // Set the workspace path to the actual root directory
        workspace.path = workspace_root.display().to_string();

        debug!(workspace = ?workspace, "Loaded workspace configuration");

        // Make sure library directory exists
        if let Some(library) = workspace.library.as_ref() {
            std::fs::create_dir_all(&library.path)?;
        }

        Ok(workspace)
    }

    /// Get a user-friendly name for the workspace
    pub fn name(&self) -> String {
        match &self.name {
            Some(name) => String::from(name),
            None => Path::new(&self.path)
                .file_stem()
                .unwrap()
                .to_os_string()
                .into_string()
                .unwrap(),
        }
    }

    /// Search the workspace for local repos matching the given pattern.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        let repos = find_git_dir(&format!("{}/{}", self.path, pattern.full_path()))?;

        // Try each remote if there were no matches immediately
        // if repos.len() == 0 {
        //     for remote in self.remotes.iter() {
        //         let repos = find_git_dir(&format!("{}/{}/{}", self.path, remote.name(), pattern.full_path()))?;
        //         if repos.len() == 0 {}
        //     }
        // }

        Ok(repos)
    }

    /// Clone/open a repository in this workspace
    pub fn open(&self, library: Option<&Library>, pattern: &RepoPattern) -> Result<PathBuf> {
        use tracing::{debug, info};

        debug!(pattern = ?pattern, "Opening repos");

        // First check if repository already exists locally
        let local_repos = self.search(pattern)?;

        if !local_repos.is_empty() {
            for repo in &local_repos {
                info!("Repository already exists: {}", repo.display());
            }
            return Ok(local_repos[0].clone());
        }

        // Check library and restore if found
        if let Some(library) = library {
            let repo_path = format!("{}/{}", self.path, pattern.full_path());
            if library.exists(repo_path.clone()) {
                info!("Restoring from library: {}", pattern.full_path());
                library.restore(repo_path.clone())?;
                return Ok(PathBuf::from(repo_path));
            }
        }

        // Try to clone from remotes
        let repo_path = self.clone_from_remote(pattern)?;

        info!("Successfully cloned to: {}", repo_path.display());
        Ok(repo_path)
    }

    /// Drop a repository from this workspace
    pub fn drop(&self, library: Option<&Library>, pattern: &RepoPattern, delete: bool, force: bool) -> Result<()> {
        use tracing::{debug, info, warn};

        debug!("Drop requested for pattern: {:?}", pattern);

        let repos = self.search(pattern)?;

        for repo in repos {
            // Check for uncommitted changes unless --force is given
            if !force && has_uncommitted_changes(&repo)? {
                warn!("Refusing to drop repository with uncommitted changes: {}", repo.display());
                warn!("Use --force to drop anyway");
                continue;
            }

            if !delete {
                if let Some(library) = library {
                    // Store the repository in the library
                    info!("Storing {} in library", repo.display());
                    library.store(repo.to_string_lossy().to_string())?;
                }
            }

            // Remove the directory
            debug!("Removing directory: {:?}", &repo);
            std::fs::remove_dir_all(repo)?;
        }
        Ok(())
    }

    /// Drop all repositories in the current directory
    pub fn drop_all(&self, library: Option<&Library>, delete: bool, force: bool) -> Result<()> {
        use tracing::{debug, info, warn};

        debug!("Drop all requested in current directory");

        let cwd = std::env::current_dir()?;
        let repos = find_git_dir(&cwd.to_string_lossy())?;

        for repo in repos {
            // Check for uncommitted changes unless --force is given
            if !force && has_uncommitted_changes(&repo)? {
                warn!("Refusing to drop repository with uncommitted changes: {}", repo.display());
                warn!("Use --force to drop anyway");
                continue;
            }

            if !delete {
                if let Some(library) = library {
                    // Store the repository in the library
                    info!("Storing {} in library", repo.display());
                    library.store(repo.to_string_lossy().to_string())?;
                }
            }

            // Remove the directory
            debug!("Removing directory: {:?}", &repo);
            std::fs::remove_dir_all(repo)?;
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
    pub fn store(&self, repo_path: String) -> Result<()> {
        // Make sure the library directory exists first
        std::fs::create_dir_all(&self.path)?;

        let source = format!("{}/.git", repo_path);
        let dest = self.compute_library_key(&repo_path);
        run_fun!(git -C $source config core.bare true)?;

        debug!(source = %source, dest = %dest, "Storing repository in library");

        // Clear the library entry if it exists
        std::fs::remove_dir_all(&dest).ok();

        run_fun!(mv $source $dest)?;
        Ok(())
    }

    pub fn restore(&self, repo_path: String) -> Result<()> {
        let source = self.compute_library_key(&repo_path);
        run_fun!(git clone $source $repo_path)?;
        Ok(())
    }

    pub fn exists(&self, repo_path: String) -> bool {
        std::fs::metadata(self.compute_library_key(&repo_path)).is_ok()
    }

    fn compute_library_key(&self, path: &str) -> String {
        format!(
            "{}/{:x}",
            self.path,
            Sha512::new().chain_update(path).finalize()
        )
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
    fn test_workspace_name() {
        let workspace = Workspace {
            name: Some("test".to_string()),
            path: "/some/path".to_string(),
            remotes: vec![],
        };
        assert_eq!(workspace.name(), "test");

        let workspace = Workspace {
            name: None,
            path: "/some/workspace".to_string(),
            remotes: vec![],
        };
        assert_eq!(workspace.name(), "workspace");
    }

    #[test]
    fn test_library_exists() {
        let temp_dir = TempDir::new().unwrap();
        let library = Library {
            path: temp_dir.path().to_string_lossy().to_string(),
        };

        let repo_path = "/test/repo";
        assert!(!library.exists(repo_path.to_string()));

        // Create the library directory
        let library_key = library.compute_library_key(repo_path);
        fs::create_dir_all(&library_key).unwrap();

        assert!(library.exists(repo_path.to_string()));
    }

    #[test]
    fn test_library_key_consistency() {
        let library = Library {
            path: "/library".to_string(),
        };

        let key1 = library.compute_library_key("/same/path");
        let key2 = library.compute_library_key("/same/path");
        assert_eq!(key1, key2);

        let key3 = library.compute_library_key("/different/path");
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_find_git_dir() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        // Create a git directory structure
        let repo1 = base_path.join("repo1");
        fs::create_dir_all(repo1.join(".git")).unwrap();

        let repo2 = base_path.join("nested/repo2");
        fs::create_dir_all(repo2.join(".git")).unwrap();

        let not_repo = base_path.join("not_a_repo");
        fs::create_dir_all(&not_repo).unwrap();

        let repos = find_git_dir(&base_path.to_string_lossy()).unwrap();

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
