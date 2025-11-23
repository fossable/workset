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

/// Represents the user's config file
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub workspace: Vec<Workspace>,

    /// The cache directory for all workspaces
    #[serde(flatten)]
    pub cache: Option<RepoCache>,
}

impl Default for Config {
    fn default() -> Self {
        let home = home::home_dir().expect("the home directory exists");

        Self {
            workspace: vec![Workspace {
                name: Some("default".into()),
                path: home.join("workspace").display().to_string(),
                remotes: vec![],
            }],
            cache: Some(RepoCache {
                path: home.join(".cache/wsx").display().to_string(),
            }),
        }
    }
}

impl Config {
    /// Load the application config from the filesystem or provide a default if
    /// none exists.
    pub fn load() -> Result<Self> {
        let config_path = match home::home_dir() {
            Some(home) => format!("{}/.config/wsx.toml", home.display()),
            None => bail!("Home directory not found"),
        };
        debug!(config_path = %config_path, "Searching for configuration file");

        let config: Config = match std::fs::metadata(&config_path) {
            Ok(_) => toml::from_str(&std::fs::read_to_string(config_path)?)?,
            Err(_) => Config::default(),
        };
        debug!(config = ?config, "Loaded configuration");

        // Make sure all necessary directories exist
        if let Some(cache) = config.cache.as_ref() {
            std::fs::create_dir_all(&cache.path)?;
        }
        for workspace in config.workspace.iter() {
            std::fs::create_dir_all(&workspace.path)?;
        }

        Ok(config)
    }

    /// Find a configured workspace by name.
    pub fn workspace_by_name(&self, workspace_name: &str) -> Option<&Workspace> {
        self.workspace.iter().find(|&w| match &w.name {
            Some(name) => name == workspace_name,
            None => false,
        })
    }

    /// Find a workspace that contains the given directory path.
    /// Returns the workspace whose path is a parent of the given directory.
    pub fn workspace_from_path(&self, dir: &Path) -> Option<&Workspace> {
        use std::path::PathBuf;

        let canonical_dir = match dir.canonicalize() {
            Ok(p) => p,
            Err(_) => return None,
        };

        // Find workspace that contains this directory
        self.workspace.iter().find(|w| {
            if let Ok(ws_path) = PathBuf::from(&w.path).canonicalize() {
                canonical_dir.starts_with(&ws_path)
            } else {
                false
            }
        })
    }

    /// Resolve a repository pattern against local repositories.
    /// Searches all workspaces since workspace is now specified separately via CLI flag.
    pub fn search_local(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        let mut all_repos = Vec::new();

        for workspace in &self.workspace {
            let repos = workspace.search(pattern)?;
            all_repos.extend(repos);
        }

        Ok(all_repos)
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

/// A `Workspace` is filesystem directory containing git repositories checked out
/// from one or more remotes. Each repository's path matches the remote's path,
/// for example:
///     <workspace path>/github.com/cilki/wsx
#[derive(Debug, Serialize, Deserialize)]
pub struct Workspace {
    /// A user-friendly name for the workspace like "personal" or "work"
    pub name: Option<String>,

    /// The workspace directory's filesystem path
    pub path: String,

    /// A list of providers for the workspace
    pub remotes: Vec<Remote>,
}

impl Workspace {
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
    pub fn open(&self, cache: Option<&RepoCache>, pattern: &RepoPattern) -> Result<PathBuf> {
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

        // Check cache and restore if found
        if let Some(cache) = cache {
            let repo_path = format!("{}/{}", self.path, pattern.full_path());
            if cache.exists(repo_path.clone()) {
                info!("Restoring from cache: {}", pattern.full_path());
                cache.uncache(repo_path.clone())?;
                return Ok(PathBuf::from(repo_path));
            }
        }

        // Try to clone from remotes
        let repo_path = self.clone_from_remote(pattern)?;

        info!("Successfully cloned to: {}", repo_path.display());
        Ok(repo_path)
    }

    /// Drop a repository from this workspace
    pub fn drop(&self, cache: Option<&RepoCache>, pattern: &RepoPattern) -> Result<()> {
        use tracing::debug;

        debug!("Drop requested for pattern: {:?}", pattern);

        let repos = self.search(pattern)?;

        for repo in repos {
            let out = run_fun!(git -C $repo status --porcelain)?;
            if out.is_empty() {
                if let Some(cache) = cache {
                    // Cache the repository
                    cache.cache(repo.to_string_lossy().to_string())?;
                }

                // Remove the directory
                debug!("Removing directory: {:?}", &repo);
                std::fs::remove_dir_all(repo)?;
            } else {
                debug!("Refusing to drop repository with uncommitted changes");
            }
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

/// Caches repositories that are dropped from a `Workspace` in a separate directory.
/// Entries in this cache are bare repositories for space efficiency.
#[derive(Debug, Serialize, Deserialize)]
pub struct RepoCache {
    pub path: String,
}

impl RepoCache {
    /// Move the given repository into the cache.
    pub fn cache(&self, repo_path: String) -> Result<()> {
        // Make sure the cache directory exists first
        std::fs::create_dir_all(&self.path)?;

        let source = format!("{}/.git", repo_path);
        let dest = self.compute_cache_key(&repo_path);
        run_fun!(git -C $source config core.bare true)?;

        debug!(source = %source, dest = %dest, "Caching repository");

        // Clear the cache entry if it exists
        std::fs::remove_dir_all(&dest).ok();

        run_fun!(mv $source $dest)?;
        Ok(())
    }

    pub fn uncache(&self, repo_path: String) -> Result<()> {
        let source = self.compute_cache_key(&repo_path);
        run_fun!(git clone $source $repo_path)?;
        Ok(())
    }

    pub fn exists(&self, repo_path: String) -> bool {
        std::fs::metadata(self.compute_cache_key(&repo_path)).is_ok()
    }

    fn compute_cache_key(&self, path: &str) -> String {
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
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.workspace.len(), 1);
        assert_eq!(config.workspace[0].name, Some("default".to_string()));
        assert!(config.cache.is_some());
    }

    #[test]
    fn test_workspace_by_name() {
        let config = Config {
            workspace: vec![
                Workspace {
                    name: Some("work".to_string()),
                    path: "/work".to_string(),
                    remotes: vec![],
                },
                Workspace {
                    name: Some("personal".to_string()),
                    path: "/personal".to_string(),
                    remotes: vec![],
                },
            ],
            cache: None,
        };

        let workspace = config.workspace_by_name("work");
        assert!(workspace.is_some());
        assert_eq!(workspace.unwrap().path, "/work");

        let workspace = config.workspace_by_name("personal");
        assert!(workspace.is_some());
        assert_eq!(workspace.unwrap().path, "/personal");

        let workspace = config.workspace_by_name("nonexistent");
        assert!(workspace.is_none());
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
    fn test_repo_cache_exists() {
        let temp_dir = TempDir::new().unwrap();
        let cache = RepoCache {
            path: temp_dir.path().to_string_lossy().to_string(),
        };

        let repo_path = "/test/repo";
        assert!(!cache.exists(repo_path.to_string()));

        // Create the cache directory
        let cache_key = cache.compute_cache_key(repo_path);
        fs::create_dir_all(&cache_key).unwrap();

        assert!(cache.exists(repo_path.to_string()));
    }

    #[test]
    fn test_repo_cache_key_consistency() {
        let cache = RepoCache {
            path: "/cache".to_string(),
        };

        let key1 = cache.compute_cache_key("/same/path");
        let key2 = cache.compute_cache_key("/same/path");
        assert_eq!(key1, key2);

        let key3 = cache.compute_cache_key("/different/path");
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

    #[test]
    fn test_workspace_from_path() {
        use std::fs;

        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();

        let config = Config {
            workspace: vec![
                Workspace {
                    name: Some("test".to_string()),
                    path: workspace_path.to_string_lossy().to_string(),
                    remotes: vec![],
                },
            ],
            cache: None,
        };

        // Test path within workspace
        let subdir = workspace_path.join("github.com/user/repo");
        fs::create_dir_all(&subdir).unwrap();

        let found_ws = config.workspace_from_path(&subdir);
        assert!(found_ws.is_some());
        assert_eq!(found_ws.unwrap().name, Some("test".to_string()));

        // Test path outside workspace
        let outside_path = temp_dir.path().join("outside");
        fs::create_dir_all(&outside_path).unwrap();

        let found_ws = config.workspace_from_path(&outside_path);
        assert!(found_ws.is_none());
    }

    #[test]
    fn test_workspace_from_path_multiple_workspaces() {
        use std::fs;

        let temp_dir = TempDir::new().unwrap();
        let ws1_path = temp_dir.path().join("workspace1");
        let ws2_path = temp_dir.path().join("workspace2");
        fs::create_dir_all(&ws1_path).unwrap();
        fs::create_dir_all(&ws2_path).unwrap();

        let config = Config {
            workspace: vec![
                Workspace {
                    name: Some("workspace1".to_string()),
                    path: ws1_path.to_string_lossy().to_string(),
                    remotes: vec![],
                },
                Workspace {
                    name: Some("workspace2".to_string()),
                    path: ws2_path.to_string_lossy().to_string(),
                    remotes: vec![],
                },
            ],
            cache: None,
        };

        // Test path in first workspace
        let subdir1 = ws1_path.join("repos");
        fs::create_dir_all(&subdir1).unwrap();

        let found_ws = config.workspace_from_path(&subdir1);
        assert!(found_ws.is_some());
        assert_eq!(found_ws.unwrap().name, Some("workspace1".to_string()));

        // Test path in second workspace
        let subdir2 = ws2_path.join("repos");
        fs::create_dir_all(&subdir2).unwrap();

        let found_ws = config.workspace_from_path(&subdir2);
        assert!(found_ws.is_some());
        assert_eq!(found_ws.unwrap().name, Some("workspace2".to_string()));
    }
}
