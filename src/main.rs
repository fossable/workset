use anyhow::Result;
use wsx::remote::ListRepos;
use wsx::Config;

/// Build info provided by built crate.
pub mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::load()?;

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
        return init_workspace(&config);
    }

    // Detect workspace from current working directory
    let workspace = match std::env::current_dir() {
        Ok(cwd) => {
            // Try to find workspace that contains current directory
            match config.workspace_from_path(&cwd) {
                Some(ws) => ws,
                None => {
                    anyhow::bail!(
                        "Current directory is not in a configured workspace.\n\
                        \n\
                        You are in: {}\n\
                        \n\
                        To create a workspace here, run:\n\
                          wsx init\n\
                        \n\
                        Or navigate to an existing workspace directory.",
                        cwd.display()
                    )
                }
            }
        }
        Err(_) => {
            anyhow::bail!("Failed to determine current directory")
        }
    };

    match args.subcommand()? {
        Some(command) => match command.as_str() {
            "init" => {
                // Already handled above, but keeping here for completeness
                init_workspace(&config)
            },
            "clone" => {
                let path = args.opt_free_from_str::<String>()?
                    .ok_or_else(|| anyhow::anyhow!("No pattern given"))?;
                let pattern: wsx::RepoPattern = path.parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                workspace.open(config.cache.as_ref(), &pattern)?;
                Ok(())
            },
            "drop" => {
                let path = args.opt_free_from_str::<String>()?
                    .ok_or_else(|| anyhow::anyhow!("No pattern given"))?;
                let pattern: wsx::RepoPattern = path.parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                workspace.drop(config.cache.as_ref(), &pattern)?;
                Ok(())
            },
            "help" => print_help(),
            _ => {
                let pattern: wsx::RepoPattern = command.parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                workspace.open(config.cache.as_ref(), &pattern)?;
                Ok(())
            },
        },
        None => {
            #[cfg(feature = "tui")]
            {
                // Open TUI for interactive repository selection
                match wsx::tui::run_tui(&config)? {
                    Some(repo) => {
                        let pattern: wsx::RepoPattern = repo.parse()
                            .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;
                        workspace.open(config.cache.as_ref(), &pattern)?;
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
fn init_workspace(config: &Config) -> Result<()> {
    use std::io::Write;

    let cwd = std::env::current_dir()?;

    // Check if we're already in a workspace
    if config.workspace_from_path(&cwd).is_some() {
        anyhow::bail!("Current directory is already part of a workspace");
    }

    let workspace_path = cwd.display().to_string();

    // Derive a name from the directory
    let default_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

    println!("Initializing workspace in: {}", workspace_path);
    println!();

    // For now, we'll add it to the config file
    let config_path = match home::home_dir() {
        Some(home) => format!("{}/.config/wsx.toml", home.display()),
        None => anyhow::bail!("Home directory not found"),
    };

    // Read existing config or create new one
    let mut updated_config = if std::fs::metadata(&config_path).is_ok() {
        toml::from_str::<Config>(&std::fs::read_to_string(&config_path)?)?
    } else {
        Config {
            workspace: vec![],
            cache: Some(wsx::RepoCache {
                path: home::home_dir()
                    .unwrap()
                    .join(".cache/wsx")
                    .display()
                    .to_string(),
            }),
        }
    };

    // Add new workspace
    updated_config.workspace.push(wsx::Workspace {
        name: Some(default_name.clone()),
        path: workspace_path.clone(),
        remotes: vec![],
    });

    // Ensure config directory exists
    if let Some(parent) = std::path::Path::new(&config_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write updated config
    let toml_string = toml::to_string_pretty(&updated_config)?;
    let mut file = std::fs::File::create(&config_path)?;
    file.write_all(toml_string.as_bytes())?;

    println!("✓ Added workspace '{}' to {}", default_name, config_path);
    println!();
    println!("You can now use wsx commands in this directory.");
    println!();
    println!("To configure remotes, edit: {}", config_path);

    Ok(())
}

/// Output help text.
fn print_help() -> Result<()> {
    println!(
        "wsx {} ({})",
        build_info::PKG_VERSION,
        build_info::BUILT_TIME_UTC
    );
    println!();
    println!("USAGE:");
    println!("  wsx init");
    println!("  wsx <repo pattern>");
    println!("  wsx clone <repo pattern>");
    println!("  wsx drop [repo pattern]");
    println!();
    println!("DESCRIPTION:");
    println!("  The workspace is automatically detected from your current directory.");
    println!();
    println!("COMMANDS:");
    println!("  init                           Initialize a workspace in current directory");
    println!("  clone <pattern>                Clone a repository (same as default)");
    println!("  drop <pattern>                 Drop a repository from workspace");
    println!();
    println!("EXAMPLES:");
    println!("  wsx init                       Initialize workspace here");
    println!("  wsx github.com/user/repo       Clone a repository");
    println!("  wsx drop github.com/user/repo  Drop a repository");
    Ok(())
}

/// Get repository completions from configured remotes
fn get_repo_completions(config: &Config) -> Vec<String> {
    let mut repos = Vec::new();

    for workspace in &config.workspace {
        for remote in &workspace.remotes {
            if let Ok(paths) = remote.list_repo_paths() {
                repos.extend(paths);
            }
        }
    }

    repos.sort();
    repos.dedup();
    repos
}

/// Output dynamic completions for bash
fn complete_bash() -> Result<()> {
    let config = Config::load()?;
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
        for repo in get_repo_completions(&config) {
            println!("{}", repo);
        }
    }

    Ok(())
}

/// Output dynamic completions for fish
fn complete_fish() -> Result<()> {
    let config = Config::load()?;
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
        for repo in get_repo_completions(&config) {
            println!("{}", repo);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "github")]
    use wsx::remote::github::GithubRemote;
    use wsx::Workspace;

    #[test]
    #[cfg(feature = "github")]
    fn test_get_repo_completions() {
        // Create a test config with a GitHub remote
        let remote = GithubRemote {
            user: "testuser".to_string(),
            include_forks: false,
            include_archived: false,
        };

        let config = Config {
            workspace: vec![
                Workspace {
                    name: Some("test".to_string()),
                    path: "/test".to_string(),
                    remotes: vec![remote.into()],
                },
            ],
            cache: None,
        };

        // Note: This will actually try to fetch from GitHub API
        // For a proper test, we'd need to mock the HTTP calls
        let _completions = get_repo_completions(&config);
        // Can't assert much here without mocking, but at least test it doesn't panic
    }

    #[test]
    fn test_get_repo_completions_no_remotes() {
        let config = Config {
            workspace: vec![
                Workspace {
                    name: Some("test".to_string()),
                    path: "/test".to_string(),
                    remotes: vec![],
                },
            ],
            cache: None,
        };

        let completions = get_repo_completions(&config);
        assert_eq!(completions.len(), 0);
    }
}
