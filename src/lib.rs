use anyhow::{Result, bail};
use std::error::Error;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{debug, info, warn};

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
}

/// Represents a git submodule within a repository
#[derive(Debug, Clone)]
pub struct SubmoduleInfo {
    /// The submodule name from .gitmodules
    pub name: String,
    /// Relative path within parent repo
    pub path: PathBuf,
    /// Clone URL
    pub url: String,
    /// Whether submodule is checked out
    pub initialized: bool,
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

/// Find all submodules in a git repository by parsing the .gitmodules file
pub fn find_submodules_in_repo(repo_path: &Path) -> Result<Vec<SubmoduleInfo>> {
    let gitmodules_path = repo_path.join(".gitmodules");

    // If .gitmodules doesn't exist, return empty vec
    if !gitmodules_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&gitmodules_path)?;
    let mut submodules = Vec::new();

    // Simple parser for .gitmodules INI format
    let mut current_name: Option<String> = None;
    let mut current_path: Option<PathBuf> = None;
    let mut current_url: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // Parse [submodule "name"] section headers
        if line.starts_with('[') && line.ends_with(']') {
            // Save previous submodule if we have all required fields
            if let (Some(name), Some(path), Some(url)) =
                (current_name.take(), current_path.take(), current_url.take())
            {
                // Check if submodule is initialized
                let initialized = repo_path.join(&path).join(".git").exists();

                submodules.push(SubmoduleInfo {
                    name: name.clone(),
                    path,
                    url,
                    initialized,
                });
            }

            // Extract submodule name from [submodule "name"]
            if let Some(start) = line.find('"')
                && let Some(end) = line.rfind('"')
                && start < end
            {
                current_name = Some(line[start + 1..end].to_string());
            }
            continue;
        }

        // Parse key = value lines
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim();
            let value = line[eq_pos + 1..].trim();

            match key {
                "path" => current_path = Some(PathBuf::from(value)),
                "url" => current_url = Some(value.to_string()),
                _ => {} // Ignore other fields
            }
        }
    }

    // Don't forget the last submodule
    if let (Some(name), Some(path), Some(url)) = (current_name, current_path, current_url) {
        let initialized = repo_path.join(&path).join(".git").exists();

        submodules.push(SubmoduleInfo {
            name,
            path,
            url,
            initialized,
        });
    }

    Ok(submodules)
}

/// Repository status information
#[derive(Debug)]
pub enum RepoStatus {
    /// Repository is clean (has commits, no changes, no unpushed)
    Clean,
    /// Repository has uncommitted changes or untracked files
    Dirty,
    /// Repository has no commits yet
    NoCommits,
    /// Repository has unpushed commits (but is otherwise clean)
    Unpushed,
}

/// Check repository status (commits, changes, unpushed) in a single pass
pub fn check_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Failed to open repository at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(RepoStatus::NoCommits);
        }
    };
    check_repo_status_with_handle(&repo, repo_path)
}

/// Check repository status and get modification time in a single repo open
/// This is more efficient than calling check_repo_status and get_repo_modification_time separately
pub fn check_repo_status_and_modification_time(
    repo_path: &Path,
) -> Result<(RepoStatus, Option<std::time::SystemTime>)> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Failed to open repository at {}: {}",
                repo_path.display(),
                e
            );
            return Ok((RepoStatus::NoCommits, None));
        }
    };

    let status = check_repo_status_with_handle(&repo, repo_path)?;
    let is_clean = matches!(status, RepoStatus::Clean);
    let mod_time = get_repo_modification_time_with_handle(&repo, repo_path, is_clean).ok();

    Ok((status, mod_time))
}

/// Check repository status using an already-opened repository handle
fn check_repo_status_with_handle(repo: &gix::Repository, repo_path: &Path) -> Result<RepoStatus> {
    // Check if repo has commits
    let head_ref = match repo.head() {
        Ok(head) => match head.try_into_referent() {
            Some(head_ref) => head_ref,
            None => return Ok(RepoStatus::NoCommits),
        },
        Err(_) => return Ok(RepoStatus::NoCommits),
    };

    // Check for uncommitted changes using a single status call
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "Failed to create status platform at {}: {}",
                repo_path.display(),
                e
            );
            return Ok(RepoStatus::Clean);
        }
    };

    // Check both tracked changes and untracked files in one pass
    let has_changes = match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(mut iter) => iter.by_ref().flatten().next().is_some(),
        Err(e) => {
            warn!(
                "Failed to check for changes at {}: {}",
                repo_path.display(),
                e
            );
            false
        }
    };

    if has_changes {
        return Ok(RepoStatus::Dirty);
    }

    // Check for unpushed commits
    let local_branch = head_ref.name();
    let remote_ref_name =
        match repo.branch_remote_ref_name(local_branch, gix::remote::Direction::Fetch) {
            Some(Ok(name)) => name,
            Some(Err(e)) => {
                debug!("Failed to get remote ref: {}", e);
                return Ok(RepoStatus::Clean);
            }
            None => {
                debug!("No upstream branch configured");
                return Ok(RepoStatus::Clean);
            }
        };

    // Try to find the remote ref
    let has_unpushed = match repo.find_reference(remote_ref_name.as_ref()) {
        Ok(remote_ref) => {
            let local_commit = match head_ref.id().object() {
                Ok(obj) => obj.id,
                Err(e) => {
                    warn!("Failed to get local commit: {}", e);
                    return Ok(RepoStatus::Clean);
                }
            };

            let remote_commit = match remote_ref.id().object() {
                Ok(obj) => obj.id,
                Err(e) => {
                    warn!("Failed to get remote commit: {}", e);
                    return Ok(RepoStatus::Clean);
                }
            };

            local_commit != remote_commit
        }
        Err(_) => {
            debug!("Remote ref not found, assuming no unpushed commits");
            false
        }
    };

    if has_unpushed {
        Ok(RepoStatus::Unpushed)
    } else {
        Ok(RepoStatus::Clean)
    }
}

/// Format a SystemTime as a human-readable "time ago" string
pub fn format_time_ago(time: std::time::SystemTime) -> String {
    let elapsed = match std::time::SystemTime::now().duration_since(time) {
        Ok(d) => d,
        Err(_) => {
            // Time is in the future, should not happen
            return "just now".to_string();
        }
    };

    let seconds = elapsed.as_secs();

    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        // Under 1 hour: show minutes (rounded)
        let minutes = (seconds + 30) / 60; // Round to nearest minute
        format!("{}m", minutes)
    } else if seconds < 86400 {
        // Under 1 day: show hours (rounded)
        let hours = (seconds + 1800) / 3600; // Round to nearest hour
        format!("{}h", hours)
    } else if seconds < 2_592_000 {
        // Under 30 days: show days (rounded)
        let days = (seconds + 43200) / 86400; // Round to nearest day
        format!("{}d", days)
    } else if seconds < 31_536_000 {
        // Under 1 year: show months (rounded)
        let months = (seconds + 1_296_000) / 2_592_000; // Round to nearest month
        format!("{}mo", months)
    } else {
        // Over 1 year: show years (rounded)
        let years = (seconds + 15_768_000) / 31_536_000; // Round to nearest year
        format!("{}y", years)
    }
}

/// Get the last modification time for a repository
/// For clean repos, use last commit time. For dirty repos, use max of commit time or dirty files.
pub fn get_repo_modification_time(
    repo_path: &Path,
    is_clean: bool,
) -> Result<std::time::SystemTime> {
    let repo = gix::open(repo_path)?;
    get_repo_modification_time_with_handle(&repo, repo_path, is_clean)
}

/// Get the modification time using an already-opened repository handle
fn get_repo_modification_time_with_handle(
    repo: &gix::Repository,
    repo_path: &Path,
    is_clean: bool,
) -> Result<std::time::SystemTime> {
    if is_clean {
        // For clean repos, get the last commit time
        get_last_commit_time(repo)
    } else {
        // For dirty repos, get the max of last commit time and dirty file modification times
        let commit_time = get_last_commit_time(repo).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let dirty_files_time = get_dirty_files_modification_time(repo, repo_path)
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        Ok(commit_time.max(dirty_files_time))
    }
}

/// Get the last commit time using gix
fn get_last_commit_time(repo: &gix::Repository) -> Result<std::time::SystemTime> {
    let head_ref = match repo.head() {
        Ok(head) => match head.try_into_referent() {
            Some(head_ref) => head_ref,
            None => bail!("Failed to get head referent"),
        },
        Err(e) => bail!("Failed to get head: {}", e),
    };

    let commit = match head_ref.id().object() {
        Ok(obj) => obj.try_into_commit()?,
        Err(e) => bail!("Failed to get commit object: {}", e),
    };

    let commit_time = commit.time()?;
    let timestamp = commit_time.seconds;
    let system_time =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64);

    Ok(system_time)
}

/// Get the most recent modification time of dirty files
fn get_dirty_files_modification_time(
    repo: &gix::Repository,
    repo_path: &Path,
) -> Result<std::time::SystemTime> {
    let mut latest_time = std::time::SystemTime::UNIX_EPOCH;

    // Use a single status call and configure it to include both tracked changes and untracked files
    let platform = repo.status(gix::progress::Discard)?;

    // Get an iterator that includes both index-worktree changes AND untracked files
    if let Ok(iter) = platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        for item in iter.flatten() {
            let rela_path = item.rela_path();
            let file_path = repo_path.join(gix::path::from_bstr(rela_path));
            if let Ok(metadata) = std::fs::metadata(&file_path)
                && let Ok(modified) = metadata.modified()
                && modified > latest_time
            {
                latest_time = modified;
            }
        }
    }

    // Return the latest time found (could be UNIX_EPOCH if no files found)
    Ok(latest_time)
}

/// A `Workspace` is filesystem directory containing git repositories checked out
/// from one or more providers. Each repository's path matches the remote's path,
/// for example:
///     <workspace path>/github.com/fossable/workset
///
/// Workspace root is identified by the presence of a .workset/ directory.
#[derive(Debug)]
pub struct Workspace {
    /// The workspace directory's filesystem path
    pub path: String,
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
        }
    }
}

impl Workspace {
    /// Get the library path for this workspace
    pub fn library_path(&self) -> String {
        format!("{}/.workset", self.path)
    }

    /// Get the library path as a PathBuf (avoids allocation)
    fn library_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path).join(".workset")
    }

    /// Load workspace from current directory.
    pub fn load() -> Result<Option<Self>> {
        let mut workspace_root = std::env::current_dir()?;

        // Search up for a .workset/ directory
        loop {
            let workset_dir = workspace_root.join(".workset");
            if workset_dir.exists() && workset_dir.is_dir() {
                let workspace = Workspace {
                    path: workspace_root.display().to_string(),
                };

                debug!(workspace_path = %workspace.path, "Found workspace");

                // Validate the workspace configuration
                workspace.validate()?;

                // Make sure library directory exists
                std::fs::create_dir_all(workspace.library_path_buf())
                    .map_err(|e| anyhow::anyhow!("Failed to create library directory: {}", e))?;

                return Ok(Some(workspace));
            }

            // Try parent directory
            match workspace_root.parent() {
                Some(parent) => workspace_root = parent.to_path_buf(),
                None => return Ok(None),
            }
        }
    }

    /// Validate the workspace configuration
    fn validate(&self) -> Result<()> {
        // Check if workspace path exists
        if !Path::new(&self.path).exists() {
            bail!("Workspace path does not exist: {}", self.path);
        }

        Ok(())
    }

    /// Search the workspace for local repos matching the given pattern.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        let repos = find_git_repositories(&format!("{}/{}", self.path, pattern.full_path()))?;
        Ok(repos)
    }

    /// Clone/open a repository in this workspace
    pub fn open(&self, pattern: &RepoPattern) -> Result<PathBuf> {
        debug!(pattern = ?pattern, "Opening repos");

        // First check if repository already exists locally
        let local_repos = self.search(pattern)?;

        if !local_repos.is_empty() {
            let repo = &local_repos[0];
            info!("✓ Repository already in workspace: {}", repo.display());

            // Check if there are any uncommitted changes or unpushed commits
            match check_repo_status(repo)? {
                RepoStatus::Dirty => info!("  ⚠ Has uncommitted changes"),
                RepoStatus::Unpushed => info!("  ⚠ Has unpushed commits"),
                _ => {}
            }

            return Ok(local_repos[0].clone());
        }

        // Check library and restore if found
        let relative_path = pattern.full_path();
        let repo_path = format!("{}/{}", self.path, relative_path);

        if self.library_contains(&relative_path) {
            info!("📦 Restoring from library: {}", relative_path);
            self.restore_from_library(&relative_path)?;

            // Fetch latest changes from upstream
            info!("  🔄 Fetching latest changes...");
            if let Err(e) = self.fetch_updates(&PathBuf::from(&repo_path)) {
                debug!("Failed to fetch updates: {}", e);
                info!("  ⚠ Could not fetch updates (continuing anyway)");
            }

            info!("✓ Restored {}", relative_path);
            return Ok(PathBuf::from(repo_path));
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
    pub fn drop(&self, pattern: &RepoPattern, delete: bool, force: bool) -> Result<()> {
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
                match check_repo_status(&repo)? {
                    RepoStatus::Dirty => {
                        warn!(
                            "⚠ Refusing to drop repository with uncommitted changes: {}",
                            repo.display()
                        );
                        warn!("  Use --force to drop anyway");
                        continue;
                    }
                    RepoStatus::Unpushed => {
                        warn!(
                            "⚠ Refusing to drop repository with unpushed commits: {}",
                            repo.display()
                        );
                        warn!("  Use --force to drop anyway");
                        continue;
                    }
                    _ => {}
                }
            }

            if !delete {
                // Store the repository in the library using workspace-relative path
                info!("📦 Storing {} in library", repo.display());
                let relative_path = repo
                    .strip_prefix(&self.path)
                    .unwrap_or(&repo)
                    .to_string_lossy()
                    .trim_start_matches('/')
                    .to_string();
                self.store_in_library(&relative_path)?;
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
    pub fn drop_all(&self, delete: bool, force: bool) -> Result<()> {
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
                match check_repo_status(&repo)? {
                    RepoStatus::Dirty => {
                        warn!(
                            "⚠ Skipping repository with uncommitted changes: {}",
                            repo.display()
                        );
                        skipped += 1;
                        continue;
                    }
                    RepoStatus::Unpushed => {
                        warn!(
                            "⚠ Skipping repository with unpushed commits: {}",
                            repo.display()
                        );
                        skipped += 1;
                        continue;
                    }
                    _ => {}
                }
            }

            if !delete {
                // Store the repository in the library using workspace-relative path
                info!("📦 Storing {} in library", repo.display());
                let relative_path = repo
                    .strip_prefix(&self.path)
                    .unwrap_or(&repo)
                    .to_string_lossy()
                    .trim_start_matches('/')
                    .to_string();
                self.store_in_library(&relative_path)?;
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

            // Clone using gix
            let mut prepare_fetch = gix::clone::PrepareFetch::new(
                clone_url,
                std::path::Path::new(&dest_path),
                gix::create::Kind::WithWorktree,
                gix::create::Options::default(),
                gix::open::Options::isolated(),
            )?;
            let should_interrupt = std::sync::atomic::AtomicBool::new(false);
            let (mut prepare_checkout, _) =
                prepare_fetch.fetch_then_checkout(gix::progress::Discard, &should_interrupt)?;
            prepare_checkout.main_worktree(gix::progress::Discard, &should_interrupt)?;

            return Ok(PathBuf::from(dest_path));
        }

        // No provider specified, would need to check configured remotes
        bail!("No provider specified. Use full path like github.com/user/repo")
    }

    /// Check if a repository exists in the library
    pub fn library_contains(&self, repo_path: &str) -> bool {
        std::fs::metadata(format!("{}/{}", self.library_path(), repo_path)).is_ok()
    }

    /// Move the given repository into the library.
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn store_in_library(&self, relative_path: &str) -> Result<()> {
        use tracing::debug;

        let library_path = self.library_path();

        // Make sure the library directory exists first
        std::fs::create_dir_all(&library_path).map_err(|e| {
            anyhow::anyhow!("Failed to create library directory {}: {}", library_path, e)
        })?;

        let source = format!("{}/{}/.git", self.path, relative_path);
        let dest = format!("{}/{}", library_path, relative_path);

        // Verify the source .git directory exists
        if std::fs::metadata(&source).is_err() {
            bail!("Repository .git directory not found: {}", source);
        }

        // Set core.bare=true by modifying the config file directly
        let config_path = std::path::Path::new(&source).join("config");
        let config_content = std::fs::read_to_string(&config_path)?;

        // Simple approach: check if core.bare already exists and update it, or add it
        let new_config = if config_content.contains("[core]") {
            // Replace or add bare = true under [core]
            if config_content.contains("bare =") || config_content.contains("bare=") {
                config_content
                    .lines()
                    .map(|line| {
                        if line.trim().starts_with("bare") {
                            "\tbare = true".to_string()
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                // Add bare = true after [core]
                config_content.replacen("[core]", "[core]\n\tbare = true", 1)
            }
        } else {
            // Add [core] section with bare = true
            format!("{}\n[core]\n\tbare = true\n", config_content)
        };

        std::fs::write(&config_path, new_config)?;

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

        // Move the repository to the library
        std::fs::rename(&source, &dest)
            .map_err(|e| anyhow::anyhow!("Failed to move repository to library: {}", e))?;

        Ok(())
    }

    /// Restore a repository from the library to the workspace.
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn restore_from_library(&self, relative_path: &str) -> Result<()> {
        use tracing::debug;

        let library_path = self.library_path();
        let source = format!("{}/{}", library_path, relative_path);
        let dest = format!("{}/{}", self.path, relative_path);

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

        // Get all remotes from the bare repository using gix
        let source_repo = gix::open(&source)?;
        let names = source_repo.remote_names();
        let mut remote_names = Vec::new();
        for name in names.iter() {
            if let Ok(s) = std::str::from_utf8(name.as_ref()) {
                remote_names.push(s.to_string());
            }
        }

        // Clone from the library using gix
        let mut prepare_fetch = gix::clone::PrepareFetch::new(
            source.clone(),
            std::path::Path::new(&dest),
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )?;
        let should_interrupt = std::sync::atomic::AtomicBool::new(false);
        let (mut prepare_checkout, _) =
            prepare_fetch.fetch_then_checkout(gix::progress::Discard, &should_interrupt)?;
        let (_dest_repo, _) =
            prepare_checkout.main_worktree(gix::progress::Discard, &should_interrupt)?;

        // Restore all original remote URLs by updating the config file
        let dest_config_path = std::path::Path::new(&dest).join(".git/config");
        let mut dest_config_content = std::fs::read_to_string(&dest_config_path)?;

        for remote_name in &remote_names {
            // Get the URL for this remote from the library
            if let Ok(remote) = source_repo.find_remote(remote_name.as_str()) {
                if let Some(url) = remote.url(gix::remote::Direction::Fetch) {
                    let remote_url = url.to_bstring().to_string();
                    debug!("Restoring remote '{}' to: {}", remote_name, remote_url);

                    // Find and update the URL line for this remote
                    let remote_section = format!("[remote \"{}\"]", remote_name);
                    if let Some(section_start) = dest_config_content.find(&remote_section) {
                        // Find the URL line after the section start
                        if let Some(url_line_start) =
                            dest_config_content[section_start..].find("url = ")
                        {
                            let abs_url_start = section_start + url_line_start;
                            if let Some(line_end) = dest_config_content[abs_url_start..].find('\n')
                            {
                                let abs_line_end = abs_url_start + line_end;
                                dest_config_content.replace_range(
                                    abs_url_start..abs_line_end,
                                    &format!("\turl = {}", remote_url),
                                );
                            }
                        }
                    }
                }
            }
        }

        std::fs::write(&dest_config_path, dest_config_content)?;

        Ok(())
    }

    /// List all repositories in the library
    pub fn list_library(&self) -> Result<Vec<String>> {
        use tracing::debug;

        let library_path = self.library_path();
        if !Path::new(&library_path).exists() {
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

        find_repos(&library_path, Path::new(&library_path), &mut repos)?;

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
        assert!(!workspace.path.is_empty());
    }

    #[test]
    fn test_library_contains() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().to_string_lossy().to_string(),
        };

        let repo_path = "test/repo";
        assert!(!workspace.library_contains(repo_path));

        // Create the library directory with a test repo
        let library_path = format!("{}/{}", workspace.library_path(), repo_path);
        fs::create_dir_all(&library_path).unwrap();

        assert!(workspace.library_contains(repo_path));
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
