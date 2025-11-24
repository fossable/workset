use super::{ListRepos, Metadata};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Serialize, Deserialize)]
pub struct GitlabRemote {
    /// Gitlab username or group name
    pub user: String,

    /// Gitlab instance URL (default: https://gitlab.com)
    #[serde(default = "default_gitlab_url")]
    pub url: String,

    /// Include forked repositories (default: false)
    #[serde(default)]
    pub include_forks: bool,

    /// Include archived repositories (default: false)
    #[serde(default)]
    pub include_archived: bool,
}

fn default_gitlab_url() -> String {
    "https://gitlab.com".to_string()
}

impl fmt::Display for GitlabRemote {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Gitlab({})", self.user)
    }
}

impl Metadata for GitlabRemote {
    fn name(&self) -> String {
        let domain = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"))
            .unwrap_or(&self.url);
        format!("{}/{}", domain, self.user)
    }
}

impl ListRepos for GitlabRemote {
    fn list_repo_paths(&self) -> Result<Vec<String>> {
        use cmd_lib::run_fun;

        // Get domain for formatting paths
        let domain = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"))
            .unwrap_or(&self.url);

        // Use glab CLI to list projects
        // glab repo list --member --per-page 100
        // Note: glab uses GITLAB_HOST env var for custom instances
        let output = if self.url != "https://gitlab.com" {
            // For custom instances, set GITLAB_HOST
            let host = domain;
            run_fun!(
                env GITLAB_HOST=$host glab repo list --member --per-page 100
            )
        } else {
            run_fun!(
                glab repo list --member --per-page 100
            )
        }
        .context(
            "Failed to run 'glab' command. Make sure GitLab CLI is installed and authenticated",
        )?;

        // Parse the output - glab returns tab-separated values by default
        // Format: namespace/project\tdescription\t...
        let mut all_repos = Vec::new();

        for line in output.lines() {
            if line.trim().is_empty() {
                continue;
            }

            // Split on tab and get the first field (project path)
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.is_empty() {
                continue;
            }

            let project_path = parts[0];

            // For filtering, we need to fetch additional metadata
            // Since glab doesn't provide fork/archive status in list output,
            // we'll need to make individual API calls or accept all repos
            // For now, we'll include all repos and note this limitation

            // Format: {domain}/{namespace}/{project}
            all_repos.push(format!("{}/{}", domain, project_path));
        }

        // Note: Fork and archive filtering would require additional API calls
        // or a different glab command. For now, we accept all repos.
        if !self.include_forks || !self.include_archived {
            tracing::warn!(
                "GitLab fork/archive filtering not yet implemented with glab CLI. \
                 Showing all repositories."
            );
        }

        Ok(all_repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gitlab_remote_display() {
        let remote = GitlabRemote {
            user: "testuser".to_string(),
            url: "https://gitlab.com".to_string(),
            include_forks: false,
            include_archived: false,
        };
        assert_eq!(format!("{}", remote), "Gitlab(testuser)");
    }

    #[test]
    fn test_gitlab_remote_name() {
        let remote = GitlabRemote {
            user: "testuser".to_string(),
            url: "https://gitlab.com".to_string(),
            include_forks: false,
            include_archived: false,
        };
        assert_eq!(remote.name(), "gitlab.com/testuser");
    }

    #[test]
    fn test_gitlab_remote_name_custom_instance() {
        let remote = GitlabRemote {
            user: "testuser".to_string(),
            url: "https://gitlab.example.com".to_string(),
            include_forks: false,
            include_archived: false,
        };
        assert_eq!(remote.name(), "gitlab.example.com/testuser");
    }

    #[test]
    fn test_gitlab_remote_defaults() {
        let remote: GitlabRemote = serde_json::from_str(r#"{"user": "testuser"}"#).unwrap();
        assert_eq!(remote.user, "testuser");
        assert_eq!(remote.url, "https://gitlab.com");
        assert!(!remote.include_forks);
        assert!(!remote.include_archived);
    }

    #[test]
    fn test_gitlab_remote_with_custom_url() {
        let remote: GitlabRemote = serde_json::from_str(
            r#"{
                "user": "testuser",
                "url": "https://custom.gitlab.com",
                "include_forks": true,
                "include_archived": true
            }"#,
        )
        .unwrap();
        assert_eq!(remote.user, "testuser");
        assert_eq!(remote.url, "https://custom.gitlab.com");
        assert!(remote.include_forks);
        assert!(remote.include_archived);
    }
}
