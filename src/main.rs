use anyhow::Result;
use workset::remote::ListRepos;
use workset::Workspace;

/// Build info provided by built crate.
pub mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    if let Ok(shell_type) = std::env::var("_ARGCOMPLETE_") {
        return match shell_type.as_str() {
            "bash" => complete_bash(),
            "fish" => complete_fish(),
            _ => anyhow::bail!("Unsupported shell type: {}", shell_type),
        };
    }

    let mut args = pico_args::Arguments::from_env();
    if args.contains("--help") {
        return print_help();
    }

    // Check for init command first (doesn't require workspace)
    if args.subcommand()?.as_deref() == Some("init") {
        return init_workspace();
    }

    // Load workspace for all other commands
    let workspace = Workspace::load()?;

    match args.subcommand()? {
        Some(command) => match command.as_str() {
            "init" => {
                // Already handled above, but keeping here for completeness
                init_workspace()
            },
            "clone" => {
                let path = args.opt_free_from_str::<String>()?
                    .ok_or_else(|| anyhow::anyhow!("No pattern given"))?;
                let pattern: workset::RepoPattern = path.parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                workspace.open(workspace.library.as_ref(), &pattern)?;
                Ok(())
            },
            "drop" => {
                let delete_flag = args.contains("--delete");
                let force_flag = args.contains("--force");
                if let Some(path) = args.opt_free_from_str::<String>()? {
                    let pattern: workset::RepoPattern = path.parse()
                        .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                    workspace.drop(workspace.library.as_ref(), &pattern, delete_flag, force_flag)?;
                } else {
                    // Drop all repos in current directory
                    workspace.drop_all(workspace.library.as_ref(), delete_flag, force_flag)?;
                }
                Ok(())
            },
            "help" => print_help(),
            _ => {
                let pattern: workset::RepoPattern = command.parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                workspace.open(workspace.library.as_ref(), &pattern)?;
                Ok(())
            },
        },
        None => {
            #[cfg(feature = "tui")]
            {
                // Open TUI for interactive repository selection
                match workset::tui::run_tui(&workspace)? {
                    Some(repo) => {
                        let pattern: workset::RepoPattern = repo.parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        workspace.open(workspace.library.as_ref(), &pattern)?;
                        Ok(())
                    },
                    None => Ok(()),
                }
            }
            #[cfg(not(feature = "tui"))]
            {
                anyhow::bail!("No command provided. TUI feature is disabled.")
            }
        }
    }
}

/// Initialize a new workspace in the current directory
fn init_workspace() -> Result<()> {
    use std::io::Write;

    let cwd = std::env::current_dir()?;
    let config_path = cwd.join(".workset.toml");

    // Check if .workset.toml already exists
    if config_path.exists() {
        anyhow::bail!("Workspace already initialized (found .workset.toml)");
    }

    println!("Initializing workspace in: {}", cwd.display());
    println!();

    // Create a new workspace with default values
    let new_workspace = Workspace::default();

    // Write workspace to .workset.toml
    let toml_string = toml::to_string_pretty(&new_workspace)?;
    let mut file = std::fs::File::create(&config_path)?;
    file.write_all(toml_string.as_bytes())?;

    println!("✓ Created .workset.toml");
    println!();
    println!("You can now use workset commands in this directory.");
    println!();
    println!("To configure remotes, edit: {}", config_path.display());

    Ok(())
}

/// Output help text.
fn print_help() -> Result<()> {
    println!(
        "workset {} ({})",
        build_info::PKG_VERSION,
        build_info::BUILT_TIME_UTC
    );
    println!();
    println!("USAGE:");
    println!("  workset init");
    println!("  workset <repo pattern>");
    println!("  workset drop [repo pattern] [--delete] [--force]");
    println!();
    println!("DESCRIPTION:");
    println!("  The workspace is automatically detected from your current directory.");
    println!();
    println!("COMMANDS:");
    println!("  init                                 Initialize a workspace in current directory");
    println!("  <pattern>                            Add a repository to your working set");
    println!("  drop [pattern] [--delete] [--force]  Drop repository(ies) from workspace");
    println!("                                       Without pattern: drops all in current directory");
    println!("                                       With --delete: permanently delete (don't store)");
    println!("                                       With --force: drop even with uncommitted changes");
    println!();
    println!("EXAMPLES:");
    println!("  workset init                              Initialize workspace here");
    println!("  workset github.com/user/repo              Add a repository");
    println!("  workset repo                              Restore 'repo' from library");
    println!("  workset drop ./repo                       Drop repo (save to library)");
    println!("  workset drop                              Drop all repos in current dir");
    println!("  workset drop --delete ./old_repo          Permanently delete a repo");
    println!("  workset drop --force ./dirty_repo         Force drop repo with changes");
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
fn complete_bash() -> Result<()> {
    let workspace = Workspace::load()?;
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
        for cmd in ["init", "clone", "drop", "help"] {
            println!("{}", cmd);
        }
    } else {
        // Complete repository paths
        for repo in get_repo_completions(&workspace) {
            println!("{}", repo);
        }
    }

    Ok(())
}

/// Output dynamic completions for fish
fn complete_fish() -> Result<()> {
    let workspace = Workspace::load()?;
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();
    let words: Vec<&str> = comp_line.split_whitespace().collect();

    // Determine what to complete based on context
    if words.len() <= 1 || (words.len() == 2 && !comp_line.ends_with(' ')) {
        // Complete subcommands
        println!("init\tInitialize a workspace in current directory");
        println!("clone\tClone one or more repositories");
        println!("drop\tDrop one or more repositories");
        println!("help\tShow help information");
    } else {
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
    #[cfg(feature = "github")]
    use workset::remote::github::GithubRemote;
    use workset::Workspace;

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
            name: Some("test".to_string()),
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
            name: Some("test".to_string()),
            path: "/test".to_string(),
            remotes: vec![],
            library: None,
        };

        let completions = get_repo_completions(&workspace);
        assert_eq!(completions.len(), 0);
    }
}
