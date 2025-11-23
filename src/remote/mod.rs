#[cfg(feature = "github")]
use self::github::GithubRemote;
#[cfg(feature = "gitlab")]
use self::gitlab::GitlabRemote;
use anyhow::Result;
use enum_dispatch::enum_dispatch;
use serde::{Deserialize, Serialize};

#[cfg(feature = "github")]
pub mod github;
#[cfg(feature = "gitlab")]
pub mod gitlab;

#[enum_dispatch]
pub trait ListRepos {
    /// List all repository paths available to the provider.
    fn list_repo_paths(&self) -> Result<Vec<String>>;
}

#[enum_dispatch]
pub trait Metadata {
    fn name(&self) -> String;
}

#[cfg(not(any(feature = "github", feature = "gitlab")))]
#[derive(Debug, Serialize, Deserialize)]
pub struct NoneRemote;

#[cfg(not(any(feature = "github", feature = "gitlab")))]
impl ListRepos for NoneRemote {
    fn list_repo_paths(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(not(any(feature = "github", feature = "gitlab")))]
impl Metadata for NoneRemote {
    fn name(&self) -> String {
        String::new()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[enum_dispatch(ListRepos, Metadata)]
pub enum Remote {
    #[cfg(feature = "github")]
    Github(GithubRemote),
    #[cfg(feature = "gitlab")]
    Gitlab(GitlabRemote),
    #[cfg(not(any(feature = "github", feature = "gitlab")))]
    None(NoneRemote),
}
