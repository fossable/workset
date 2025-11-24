use super::{ListRepos, Metadata};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Serialize, Deserialize)]
pub struct GithubRemote {
    /// Github username or organization name
    pub user: String,

    /// Include forked repositories (default: false)
    #[serde(default)]
    pub include_forks: bool,

    /// Include archived repositories (default: false)
    #[serde(default)]
    pub include_archived: bool,
}

impl fmt::Display for GithubRemote {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Github({})", self.user)
    }
}

impl Metadata for GithubRemote {
    fn name(&self) -> String {
        format!("github.com/{}", self.user)
    }
}

impl ListRepos for GithubRemote {
    fn list_repo_paths(&self) -> Result<Vec<String>> {
        use cmd_lib::run_fun;

        // Use gh CLI to list repositories
        // gh repo list <user> --json nameWithOwner,isFork,isArchived --limit 1000
        let user = &self.user;
        let output = run_fun!(
            gh repo list $user --json nameWithOwner,isFork,isArchived --limit 1000
        )
        .context(
            "Failed to run 'gh' command. Make sure GitHub CLI is installed and authenticated",
        )?;

        // Parse JSON output
        let repos: Vec<GithubRepo> =
            serde_json::from_str(&output).context("Failed to parse gh CLI output")?;

        let mut all_repos = Vec::new();

        for repo in repos {
            // Apply filters
            if !self.include_forks && repo.is_fork {
                continue;
            }
            if !self.include_archived && repo.is_archived {
                continue;
            }

            // Format: github.com/{owner}/{repo}
            all_repos.push(format!("github.com/{}", repo.name_with_owner));
        }

        Ok(all_repos)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubRepo {
    name_with_owner: String,
    is_fork: bool,
    is_archived: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_remote_display() {
        let remote = GithubRemote {
            user: "testuser".to_string(),
            include_forks: false,
            include_archived: false,
        };
        assert_eq!(format!("{}", remote), "Github(testuser)");
    }

    #[test]
    fn test_github_remote_name() {
        let remote = GithubRemote {
            user: "octocat".to_string(),
            include_forks: false,
            include_archived: false,
        };
        assert_eq!(remote.name(), "github.com/octocat");
    }

    #[test]
    fn test_github_remote_defaults() {
        let remote: GithubRemote = serde_json::from_str(r#"{"user": "testuser"}"#).unwrap();
        assert_eq!(remote.user, "testuser");
        assert!(!remote.include_forks);
        assert!(!remote.include_archived);
    }

    #[test]
    fn test_github_remote_with_options() {
        let remote: GithubRemote = serde_json::from_str(
            r#"{
                "user": "testuser",
                "include_forks": true,
                "include_archived": true
            }"#,
        )
        .unwrap();
        assert_eq!(remote.user, "testuser");
        assert!(remote.include_forks);
        assert!(remote.include_archived);
    }
}
