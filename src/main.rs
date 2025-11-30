use anyhow::Result;
use std::io::IsTerminal;
use tracing::level_filters::LevelFilter;
use tracing::{error, info};
use workset::Workspace;

/// Build info provided by built crate.
pub mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// ANSI color codes
mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const DIM: &str = "\x1b[2m";
}

/// Clone repositories matching the pattern
fn clone_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<()> {
    use std::path::PathBuf;
    use std::process::Command;

    // Check if pattern is for mass cloning from github.com or gitlab.com
    if let Some((provider, path)) = pattern.provider_and_path() {
        // Check if this is a partial path for mass cloning
        if (provider == "github.com" || provider == "gitlab.com") && !path.contains('/') {
            // This is a user/org pattern like "github.com/user" - use gh/glab to mass clone
            info!(
                "🔄 Fetching list of repositories from {}/{}...",
                provider, path
            );

            // Get list of repos using gh/glab
            let output = if provider == "github.com" {
                Command::new("gh")
                    .args([
                        "repo",
                        "list",
                        path,
                        "--json",
                        "nameWithOwner",
                        "--limit",
                        "1000",
                    ])
                    .output()
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to run 'gh'. Is it installed? Error: {}", e)
                    })?
            } else {
                Command::new("glab")
                    .args(["repo", "list", path, "--page", "1", "--per-page", "100"])
                    .output()
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to run 'glab'. Is it installed? Error: {}", e)
                    })?
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("Failed to fetch repository list: {}", stderr);
            }

            // Parse the output
            let repos = if provider == "github.com" {
                // Parse JSON output from gh
                let json_str = String::from_utf8(output.stdout)?;
                let repos_json: Vec<serde_json::Value> = serde_json::from_str(&json_str)?;
                repos_json
                    .iter()
                    .filter_map(|r| r["nameWithOwner"].as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            } else {
                // Parse glab output (simple list format)
                String::from_utf8(output.stdout)?
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
            };

            if repos.is_empty() {
                info!("No repositories found for {}/{}", provider, path);
                return Ok(());
            }

            info!("Found {} repositories. Cloning...", repos.len());

            let mut cloned = 0;
            let mut skipped = 0;

            for repo in repos {
                let repo_pattern: workset::RepoPattern =
                    format!("{}/{}", provider, repo)
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Failed to parse repo pattern: {}", e))?;

                // Check if repo already exists in workspace
                let repo_path = PathBuf::from(&workspace.path).join(repo_pattern.full_path());
                if repo_path.exists() {
                    info!("⊘ Skipping {} (already exists)", repo_pattern.full_path());
                    skipped += 1;
                    continue;
                }

                // Clone the individual repo
                match clone_single_repo(workspace, &repo_pattern) {
                    Ok(_) => {
                        cloned += 1;
                    }
                    Err(e) => {
                        error!("✗ Failed to clone {}: {}", repo_pattern.full_path(), e);
                    }
                }
            }

            info!("✓ Cloned {} repositories ({} skipped)", cloned, skipped);
            return Ok(());
        }
    }

    // Not a mass clone pattern, just clone the single repo
    clone_single_repo(workspace, pattern)?;
    Ok(())
}

/// Clone a single repository
fn clone_single_repo(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<()> {
    use std::path::PathBuf;

    let repo_path = PathBuf::from(&workspace.path).join(pattern.full_path());

    // Check if repo already exists in workspace
    if repo_path.exists() {
        info!("✓ Repository already exists: {}", pattern.full_path());
        return Ok(());
    }

    // Check if it exists in library first
    if workspace.library_contains(&pattern.full_path()) {
        info!("📦 Repository found in library, use 'restore' instead");
        return Ok(());
    }

    // Clone from remote
    if let Some((provider, repo_path_str)) = pattern.provider_and_path() {
        let clone_url = format!("https://{}/{}", provider, repo_path_str);
        let dest_path = format!("{}/{}", workspace.path, pattern.full_path());

        // Create parent directories
        if let Some(parent) = std::path::Path::new(&dest_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        info!("🔄 Cloning {}...", pattern.full_path());

        // TODO show progress
        let mut prepare_fetch = gix::clone::PrepareFetch::new(
            clone_url,
            std::path::Path::new(&dest_path),
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )?;

        let should_interrupt = std::sync::atomic::AtomicBool::new(false);
        let (mut prepare_checkout, _fetch_outcome) =
            prepare_fetch.fetch_then_checkout(gix::progress::Discard, &should_interrupt)?;

        let (_repo, _checkout_outcome) =
            prepare_checkout.main_worktree(gix::progress::Discard, &should_interrupt)?;

        info!("✓ Cloned {}", pattern.full_path());
        Ok(())
    } else {
        anyhow::bail!("No provider specified. Use format like github.com/user/repo");
    }
}

/// Restore repositories from library matching the pattern
fn restore_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<()> {
    use std::path::PathBuf;

    // Get all repos from library
    let library_repos = workspace.list_library()?;

    if library_repos.is_empty() {
        info!("Library is empty");
        return Ok(());
    }

    // Filter repos that match the pattern
    let pattern_str = pattern.full_path();
    let matching_repos: Vec<String> = library_repos
        .iter()
        .filter(|repo| repo.contains(&pattern_str))
        .cloned()
        .collect();

    if matching_repos.is_empty() {
        info!("No repositories found in library matching: {}", pattern_str);
        info!("Available repositories in library:");
        for repo in library_repos.iter().take(10) {
            info!("  - {}", repo);
        }
        if library_repos.len() > 10 {
            info!("  ... and {} more", library_repos.len() - 10);
        }
        return Ok(());
    }

    info!(
        "Found {} matching repository(ies) in library",
        matching_repos.len()
    );

    let mut restored = 0;
    let mut skipped = 0;

    for repo_path in matching_repos {
        // Check if already exists in workspace
        let dest_path = PathBuf::from(&workspace.path).join(&repo_path);
        if dest_path.exists() {
            info!("⊘ Skipping {} (already in workspace)", repo_path);
            skipped += 1;
            continue;
        }

        // Restore from library
        info!("📦 Restoring {}...", repo_path);
        match workspace.restore_from_library(&repo_path) {
            Ok(_) => {
                info!("✓ Restored {}", repo_path);
                restored += 1;
            }
            Err(e) => {
                error!("✗ Failed to restore {}: {}", repo_path, e);
            }
        }
    }

    info!(
        "✓ Restored {} repository(ies) ({} skipped)",
        restored, skipped
    );
    Ok(())
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        )
        .init();

    // Load workspace for completions or for a subcommand
    let maybe_workspace = Workspace::load()?;

    // Dispatch shell completions
    // TODO this is wrong
    if let Ok(shell_type) = std::env::var("_ARGCOMPLETE_") {
        return match shell_type.as_str() {
            "bash" => complete_bash(maybe_workspace),
            "fish" => complete_fish(maybe_workspace),
            _ => anyhow::bail!("Unsupported shell type: {}", shell_type),
        };
    }

    let mut args = pico_args::Arguments::from_env();
    if args.contains("--help") {
        let is_tty = std::io::stdout().is_terminal();

        let help = format!(
            r#"{workset} {version} ({built})

{desc_header}
  Manage git repos with working sets.

{usage_header}
  {cmd}workset{reset} init
  {cmd}workset{reset} clone <repo pattern>
  {cmd}workset{reset} restore <repo pattern>
  {cmd}workset{reset} drop [repo pattern] [--delete] [--force]

{commands_header}
  {subcmd}init{reset}                                 Initialize a workspace in current directory
  {subcmd}clone{reset} {arg}<pattern>{reset}                      Clone new repository(ies) to workspace
  {subcmd}restore{reset} {arg}<pattern>{reset}                    Restore repository(ies) from library
  {subcmd}drop{reset} {arg}[pattern]{reset} {arg}[--delete]{reset} {arg}[--force]{reset}  Drop repository(ies) from workspace
{dim}                                       Without pattern: drops all in current directory
                                       With --delete: permanently delete (don't store)
                                       With --force: drop even with uncommitted changes{reset}
  {subcmd}list{reset}, {subcmd}ls{reset}                             List all repositories with their status
  {subcmd}status{reset}                               Show workspace summary and statistics

{examples_header}
  {cmd}workset init{reset}                              Initialize workspace here
  {cmd}workset clone github.com/user/repo{reset}        Clone a new repository
  {cmd}workset clone github.com/user{reset}             Clone all repos from github.com/user
  {cmd}workset restore repo{reset}                      Restore 'repo' from library
  {cmd}workset drop ./repo{reset}                       Drop repo (save to library)
  {cmd}workset drop{reset}                              Drop all repos in current dir
  {cmd}workset drop --delete ./old_repo{reset}          Permanently delete a repo
  {cmd}workset drop --force ./dirty_repo{reset}         Force drop repo and lose any changes
"#,
            workset = if is_tty {
                format!("{}{}{}", colors::BOLD, "workset", colors::RESET)
            } else {
                "workset".to_string()
            },
            version = if is_tty {
                format!(
                    "{}{}{}",
                    colors::CYAN,
                    build_info::PKG_VERSION,
                    colors::RESET
                )
            } else {
                build_info::PKG_VERSION.to_string()
            },
            built = build_info::BUILT_TIME_UTC,
            desc_header = if is_tty {
                format!("{}{}{}", colors::BOLD, "DESCRIPTION:", colors::RESET)
            } else {
                "DESCRIPTION:".to_string()
            },
            usage_header = if is_tty {
                format!("{}{}{}", colors::BOLD, "USAGE:", colors::RESET)
            } else {
                "USAGE:".to_string()
            },
            commands_header = if is_tty {
                format!("{}{}{}", colors::BOLD, "COMMANDS:", colors::RESET)
            } else {
                "COMMANDS:".to_string()
            },
            examples_header = if is_tty {
                format!("{}{}{}", colors::BOLD, "EXAMPLES:", colors::RESET)
            } else {
                "EXAMPLES:".to_string()
            },
            cmd = if is_tty { colors::GREEN } else { "" },
            subcmd = if is_tty { colors::CYAN } else { "" },
            arg = if is_tty { colors::YELLOW } else { "" },
            dim = if is_tty { colors::DIM } else { "" },
            reset = if is_tty { colors::RESET } else { "" },
        );
        print!("{}", help);

        return Ok(());
    }

    // Dispatch subcommands
    match args.subcommand()? {
        Some(command) => match command.as_str() {
            "init" => {
                let workspace_path = std::env::current_dir()?;
                let library_path = workspace_path.join(".workset");

                if library_path.exists() {
                    info!(
                        "✓ Workspace already initialized at {}",
                        workspace_path.display()
                    );
                } else {
                    std::fs::create_dir_all(&library_path)?;
                    info!("✓ Initialized workspace at {}", workspace_path.display());
                    info!("  Library: {}", library_path.display());
                }
            }
            "clone" => {
                if let Some(workspace) = maybe_workspace {
                    if let Some(pattern_str) = args.opt_free_from_str::<String>()? {
                        let pattern: workset::RepoPattern = pattern_str
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        clone_repos(&workspace, &pattern)?;
                    } else {
                        error!("Missing repository pattern for clone command");
                        error!("Usage: workset clone <pattern>");
                    }
                } else {
                    error!("You're not in a workspace");
                }
            }
            "restore" => {
                if let Some(workspace) = maybe_workspace {
                    if let Some(pattern_str) = args.opt_free_from_str::<String>()? {
                        let pattern: workset::RepoPattern = pattern_str
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        restore_repos(&workspace, &pattern)?;
                    } else {
                        error!("Missing repository pattern for restore command");
                        error!("Usage: workset restore <pattern>");
                    }
                } else {
                    error!("You're not in a workspace");
                }
            }
            "drop" => {
                if let Some(workspace) = maybe_workspace {
                    let delete = args.contains("--delete");
                    let force = args.contains("--force");

                    if let Some(path) = args.opt_free_from_str::<String>()? {
                        let pattern: workset::RepoPattern = path
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        workspace.drop(&pattern, delete, force)?;
                    } else {
                        // Drop all repos in current directory
                        workspace.drop_all(delete, force)?;
                    }
                } else {
                    error!("You're not in a workspace");
                }
            }
            "list" | "ls" => {
                if let Some(workspace) = maybe_workspace {
                    list_workspace_status(&workspace)?;
                } else {
                    error!("You're not in a workspace");
                }
            }
            "status" => {
                if let Some(workspace) = maybe_workspace {
                    show_workspace_summary(&workspace)?;
                } else {
                    error!("You're not in a workspace");
                }
            }
            _ => {
                error!("Unknown command: {}", command);
                error!("Run 'workset --help' for usage information");
            }
        },
        None => {
            #[cfg(feature = "tui")]
            {
                if let Some(workspace) = maybe_workspace {
                    // Open TUI for interactive workspace management
                    workset::tui::run_tui(&workspace)?;
                } else {
                    error!("You're not in a workspace");
                }
            }
            #[cfg(not(feature = "tui"))]
            {
                anyhow::bail!("No command provided. TUI feature is disabled.")
            }
        }
    }
    Ok(())
}

/// List all repositories in the workspace with their status
fn list_workspace_status(workspace: &Workspace) -> Result<()> {
    let repos = workset::find_git_repositories(&workspace.path)?;

    if repos.is_empty() {
        println!("No repositories found in workspace");
        return Ok(());
    }

    println!("Repositories in workspace ({}):", workspace.path);
    println!();

    for repo in repos {
        let repo_name = repo
            .strip_prefix(&workspace.path)
            .unwrap_or(&repo)
            .display()
            .to_string();

        let status_str = match workset::check_repo_status(&repo) {
            Ok(workset::RepoStatus::Clean) => "✓ clean".to_string(),
            Ok(workset::RepoStatus::Dirty) => "⚠ modified".to_string(),
            Ok(workset::RepoStatus::Unpushed) => "⚠ unpushed".to_string(),
            Ok(workset::RepoStatus::NoCommits) => "⚠ no commits".to_string(),
            Err(_) => "✗ error".to_string(),
        };

        println!("  {} - {}", repo_name, status_str);
    }

    Ok(())
}

/// Show a summary of the workspace
fn show_workspace_summary(workspace: &Workspace) -> Result<()> {
    println!("Workspace: {}", workspace.path);
    println!();

    // Show library information
    println!("Library: {}", workspace.library_path());
    if let Ok(repos) = workspace.list_library() {
        println!("  {} repository(ies) in library", repos.len());
    }

    // Count repositories in workspace
    println!();
    if let Ok(repos) = workset::find_git_repositories(&workspace.path) {
        println!("Active repositories: {}", repos.len());

        let mut clean = 0;
        let mut modified = 0;
        let mut unpushed = 0;

        for repo in &repos {
            match workset::check_repo_status(repo) {
                Ok(workset::RepoStatus::Clean) => clean += 1,
                Ok(workset::RepoStatus::Dirty) => modified += 1,
                Ok(workset::RepoStatus::Unpushed) => unpushed += 1,
                Ok(workset::RepoStatus::NoCommits) => modified += 1,
                Err(_) => {}
            }
        }

        if clean > 0 {
            println!("  ✓ {} clean", clean);
        }
        if modified > 0 {
            println!("  ⚠ {} with uncommitted changes", modified);
        }
        if unpushed > 0 {
            println!("  ⚠ {} with unpushed commits", unpushed);
        }
    }

    Ok(())
}

/// Get repository completions from configured remotes
fn get_repo_completions(workspace: &Workspace) -> Vec<String> {
    let mut repos = Vec::new();

    // Only complete with local workspace repos
    if let Ok(local_repos) = workset::find_git_repositories(&workspace.path) {
        for repo in local_repos {
            if let Ok(relative) = repo.strip_prefix(&workspace.path) {
                repos.push(relative.display().to_string());
            }
        }
    }

    repos.sort();
    repos.dedup();
    repos
}

/// Get repository completions with metadata (status and modification time) for fish shell
fn get_repo_completions_with_metadata(workspace: &Workspace) -> Vec<(String, String)> {
    let mut repos = Vec::new();

    // Only complete with local workspace repos
    if let Ok(local_repos) = workset::find_git_repositories(&workspace.path) {
        for repo in local_repos {
            if let Ok(relative) = repo.strip_prefix(&workspace.path) {
                let repo_name = relative.display().to_string();

                // Get repo status and modification time
                // If these fail, we'll still provide a basic completion
                let status = workset::check_repo_status(&repo).ok();
                let is_clean = matches!(status, Some(workset::RepoStatus::Clean));
                let mod_time = workset::get_repo_modification_time(&repo, is_clean).ok();

                // Build description with status and time
                let mut desc_parts = Vec::new();

                // Add status indicator
                match status {
                    Some(workset::RepoStatus::Clean) => desc_parts.push("clean".to_string()),
                    Some(workset::RepoStatus::Dirty) => desc_parts.push("dirty".to_string()),
                    Some(workset::RepoStatus::Unpushed) => desc_parts.push("unpushed".to_string()),
                    Some(workset::RepoStatus::NoCommits) => {
                        desc_parts.push("no commits".to_string())
                    }
                    None => {} // Don't add "unknown" if status check failed
                }

                // Add modification time
                if let Some(time) = mod_time {
                    desc_parts.push(workset::format_time_ago(time));
                }

                // If we couldn't get any metadata, use a default description
                let description = if desc_parts.is_empty() {
                    "repository".to_string()
                } else {
                    desc_parts.join(", ")
                };

                repos.push((repo_name, description));
            }
        }
    }

    // Sort by repo name
    repos.sort_by(|a, b| a.0.cmp(&b.0));
    repos.dedup();
    repos
}

/// Output dynamic completions for bash
fn complete_bash(maybe_workspace: Option<Workspace>) -> Result<()> {
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();
    let comp_point = std::env::var("COMP_POINT")
        .unwrap_or_default()
        .parse::<usize>()
        .unwrap_or(0);

    let current_line = &comp_line[..comp_point];
    let words: Vec<&str> = current_line.split_whitespace().collect();

    // Determine what to complete based on context
    if words.len() <= 1 {
        // Complete subcommands
        if maybe_workspace.is_some() {
            println!("clone");
            println!("restore");
            println!("drop");
            println!("list");
            println!("ls");
            println!("status");
        } else {
            println!("init");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths based on the subcommand
        let subcommand = words.get(1).unwrap_or(&"");
        if *subcommand == "restore" {
            // For restore, complete from library
            if let Ok(library_repos) = workspace.list_library() {
                for repo in library_repos {
                    println!("{}", repo);
                }
            }
        } else {
            // For drop and other commands, complete from workspace
            for repo in get_repo_completions(&workspace) {
                println!("{}", repo);
            }
        }
    }

    Ok(())
}

/// Output dynamic completions for fish
fn complete_fish(maybe_workspace: Option<Workspace>) -> Result<()> {
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();
    let words: Vec<&str> = comp_line.split_whitespace().collect();

    // Determine what to complete based on context
    if words.len() <= 1 || (words.len() == 2 && !comp_line.ends_with(' ')) {
        // Complete subcommands
        if maybe_workspace.is_some() {
            println!("clone\tClone new repository(ies) to workspace");
            println!("restore\tRestore repository(ies) from library");
            println!("drop\tDrop one or more repositories");
            println!("list\tList all repositories with their status");
            println!("ls\tList all repositories with their status");
            println!("status\tShow workspace summary and statistics");
        } else {
            println!("init\tInitialize a workspace in current directory");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths based on the subcommand
        let subcommand = words.get(1).unwrap_or(&"");
        if *subcommand == "restore" {
            // For restore, complete from library
            if let Ok(library_repos) = workspace.list_library() {
                for repo in library_repos {
                    println!("{}\tlibrary", repo);
                }
            }
        } else {
            // For drop and other commands, complete from workspace with metadata
            for (repo_name, description) in get_repo_completions_with_metadata(&workspace) {
                println!("{}\t{}", repo_name, description);
            }
        }
    }

    Ok(())
}
