use anyhow::Result;
use std::io::Write;
use tracing::level_filters::LevelFilter;
use tracing::{error, info};
use workset::Workspace;
use workset::remote::ListRepos;

/// Build info provided by built crate.
pub mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // Load workspace for completions or for a subcommand
    let maybe_workspace = Workspace::load()?;

    // Dispatch shell completions
    if let Ok(shell_type) = std::env::var("_ARGCOMPLETE_") {
        return match shell_type.as_str() {
            "bash" => complete_bash(maybe_workspace),
            "fish" => complete_fish(maybe_workspace),
            _ => anyhow::bail!("Unsupported shell type: {}", shell_type),
        };
    }

    let mut args = pico_args::Arguments::from_env();
    if args.contains("--help") {
        println!(
            "workset {} ({})",
            build_info::PKG_VERSION,
            build_info::BUILT_TIME_UTC
        );
        println!();
        println!("DESCRIPTION:");
        println!("  Manage git repos with working sets.");
        println!();
        println!("USAGE:");
        println!("  workset init");
        println!("  workset <repo pattern>");
        println!("  workset drop [repo pattern] [--delete] [--force]");
        println!();
        println!("COMMANDS:");
        println!(
            "  init                                 Initialize a workspace in current directory"
        );
        println!("  <pattern>                            Add a repository to your working set");
        println!("  drop [pattern] [--delete] [--force]  Drop repository(ies) from workspace");
        println!(
            "                                       Without pattern: drops all in current directory"
        );
        println!(
            "                                       With --delete: permanently delete (don't store)"
        );
        println!(
            "                                       With --force: drop even with uncommitted changes"
        );
        println!("  list, ls                             List all repositories with their status");
        println!("  status                               Show workspace summary and statistics");
        println!();
        println!("EXAMPLES:");
        println!("  workset init                              Initialize workspace here");
        println!("  workset github.com/user/repo              Add a repository");
        println!("  workset repo                              Restore 'repo' from library");
        println!("  workset drop ./repo                       Drop repo (save to library)");
        println!("  workset drop                              Drop all repos in current dir");
        println!("  workset drop --delete ./old_repo          Permanently delete a repo");
        println!("  workset drop --force ./dirty_repo         Force drop repo with changes");
        return Ok(());
    }

    // Dispatch subcommands
    match args.subcommand()? {
        Some(command) => match command.as_str() {
            "init" => {
                let workspace = if let Some(workspace) = maybe_workspace {
                    info!("Workspace already exists");
                    // TODO allow args to change config
                    workspace
                } else {
                    info!("Creating workspace");
                    Workspace::default()
                };

                // Write or rewrite the config
                let toml_string = toml::to_string_pretty(&workspace)?;
                let mut file =
                    std::fs::File::create(std::env::current_dir()?.join(".workset.toml"))?;
                file.write_all(toml_string.as_bytes())?;
            }
            "drop" => {
                if let Some(workspace) = maybe_workspace {
                    let delete = args.contains("--delete");
                    let force = args.contains("--force");

                    if let Some(path) = args.opt_free_from_str::<String>()? {
                        let pattern: workset::RepoPattern = path
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        workspace.drop(workspace.library.as_ref(), &pattern, delete, force)?;
                    } else {
                        // Drop all repos in current directory
                        workspace.drop_all(workspace.library.as_ref(), delete, force)?;
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
                if let Some(workspace) = maybe_workspace {
                    let pattern: workset::RepoPattern = command
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                    workspace.open(workspace.library.as_ref(), &pattern)?;
                } else {
                    error!("You're not in a workspace");
                }
            }
        },
        None => {
            #[cfg(feature = "tui")]
            {
                if let Some(workspace) = maybe_workspace {
                    // Open TUI for interactive repository selection
                    if let Some(repo) = workset::tui::run_tui(&workspace)? {
                        let pattern: workset::RepoPattern = repo
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        workspace.open(workspace.library.as_ref(), &pattern)?;
                    }
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

        let mut status_flags = Vec::new();

        // Check for uncommitted changes
        if let Ok(true) = workset::check_uncommitted_changes(&repo) {
            status_flags.push("modified");
        }

        // Check for unpushed commits
        if let Ok(true) = workset::check_unpushed_commits(&repo) {
            status_flags.push("unpushed");
        }

        let status_str = if status_flags.is_empty() {
            "✓ clean".to_string()
        } else {
            format!("⚠ {}", status_flags.join(", "))
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
    if let Some(library) = &workspace.library {
        println!("Library: {}", library.path);
        if let Ok(repos) = library.list() {
            println!("  {} repository(ies) in library", repos.len());
        }
    }

    // Show configured remotes
    if !workspace.remotes.is_empty() {
        println!();
        println!("Configured remotes:");
        for (idx, remote) in workspace.remotes.iter().enumerate() {
            println!("  {}. {:?}", idx + 1, remote);
        }
    }

    // Count repositories in workspace
    println!();
    if let Ok(repos) = workset::find_git_repositories(&workspace.path) {
        println!("Active repositories: {}", repos.len());

        let mut clean = 0;
        let mut modified = 0;
        let mut unpushed = 0;

        for repo in &repos {
            let has_changes = workset::check_uncommitted_changes(repo).unwrap_or(false);
            let has_unpushed = workset::check_unpushed_commits(repo).unwrap_or(false);

            if !has_changes && !has_unpushed {
                clean += 1;
            }
            if has_changes {
                modified += 1;
            }
            if has_unpushed {
                unpushed += 1;
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

    for remote in &workspace.remotes {
        if let Ok(paths) = remote.list_repo_paths() {
            repos.extend(paths);
        }
    }

    repos.sort();
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
            println!("drop");
        } else {
            println!("init");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths
        for repo in get_repo_completions(&workspace) {
            println!("{}", repo);
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
            println!("drop\tDrop one or more repositories");
        } else {
            println!("init\tInitialize a workspace in current directory");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths
        for repo in get_repo_completions(&workspace) {
            println!("{}", repo);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workset::Workspace;
    #[cfg(feature = "github")]
    use workset::remote::github::GithubRemote;

    #[test]
    #[cfg(feature = "github")]
    fn test_get_repo_completions() {
        // Create a test workspace with a GitHub remote
        let remote = GithubRemote {
            user: "testuser".to_string(),
            include_forks: false,
            include_archived: false,
        };

        let workspace = Workspace {
            path: "/test".to_string(),
            remotes: vec![remote.into()],
            library: None,
        };

        // Note: This will actually try to fetch from GitHub API
        // For a proper test, we'd need to mock the HTTP calls
        let _completions = get_repo_completions(&workspace);
        // Can't assert much here without mocking, but at least test it doesn't panic
    }

    #[test]
    fn test_get_repo_completions_no_remotes() {
        let workspace = Workspace {
            path: "/test".to_string(),
            remotes: vec![],
            library: None,
        };

        let completions = get_repo_completions(&workspace);
        assert_eq!(completions.len(), 0);
    }
}
