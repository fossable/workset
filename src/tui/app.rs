use super::tree::{
    RepoInfo, RepoOperationStatus, TreeNode, TreeState, build_library_tree, build_tree,
    count_repos_in_trees, flatten_trees, toggle_node_at_path,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    CloneRepo,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Workspace,
    Library,
}

impl Section {
    fn other(self) -> Self {
        match self {
            Section::Workspace => Section::Library,
            Section::Library => Section::Workspace,
        }
    }
}

/// A clone running in the background, shown as a temporary repo row in the
/// workspace tree until the real repo appears (or the failure expires)
struct PendingClone {
    display_name: String,
    status: RepoOperationStatus,
    failed_at: Option<Instant>,
}

pub struct App {
    workspace_tree: Vec<TreeNode>,
    library_tree: Vec<TreeNode>,
    workspace_repos_list: Vec<RepoInfo>,
    library_repos_list: Vec<RepoInfo>,
    pub filtered_workspace: Vec<TreeNode>,
    pub filtered_library: Vec<TreeNode>,
    pub workspace_state: TreeState,
    pub library_state: TreeState,
    pub search_query: String,
    pub active_section: Section,
    pub matcher: SkimMatcherV2,
    pub workspace_path: String,
    pub loading_progress: Option<String>,
    pub watch_disabled: bool,
    pub mode: AppMode,
    pub clone_repo_input: String,
    pub clone_repo_suggestions: Vec<String>,
    pub clone_repo_state: TreeState,
    pub suggestions_loading: bool,
    pending_clones: Vec<PendingClone>,
    /// Whether the current selection was made automatically (not by the user).
    /// Automatic selections may be replaced when repo data is reloaded; user
    /// selections are preserved.
    selection_is_auto: bool,
}

impl App {
    pub fn new(
        workspace_path: String,
        workspace_repos: Vec<RepoInfo>,
        library_repos: Vec<RepoInfo>,
    ) -> Self {
        let mut app = Self {
            workspace_tree: Vec::new(),
            library_tree: Vec::new(),
            workspace_repos_list: Vec::new(),
            library_repos_list: Vec::new(),
            filtered_workspace: Vec::new(),
            filtered_library: Vec::new(),
            workspace_state: TreeState::new(),
            library_state: TreeState::new(),
            search_query: String::new(),
            active_section: Section::Workspace,
            matcher: SkimMatcherV2::default(),
            workspace_path,
            loading_progress: None,
            watch_disabled: false,
            mode: AppMode::Normal,
            clone_repo_input: String::new(),
            clone_repo_suggestions: Vec::new(),
            clone_repo_state: TreeState::new(),
            suggestions_loading: false,
            pending_clones: Vec::new(),
            selection_is_auto: true,
        };
        app.update_repos(workspace_repos, library_repos);
        app
    }

    pub fn filter_repos(&mut self) {
        self.rebuild_filtered();
        self.select_first_available();
    }

    /// Rebuild the filtered trees from the repo lists and the current search query
    fn rebuild_filtered(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_workspace = self.workspace_tree.clone();
            self.filtered_library = self.library_tree.clone();
        } else {
            // Filter repos by fuzzy-matching the search query
            let workspace_repos = self.workspace_repos_with_pending();
            let matches = |r: &&RepoInfo| {
                self.matcher
                    .fuzzy_match(&r.display_name, &self.search_query)
                    .is_some()
            };
            self.filtered_workspace =
                build_tree(workspace_repos.iter().filter(matches).cloned().collect());
            self.filtered_library = build_library_tree(
                self.library_repos_list
                    .iter()
                    .filter(matches)
                    .cloned()
                    .collect(),
                &self.workspace_repos_list,
            );
        }
    }

    /// The workspace repos with each pending clone applied: repos that already
    /// exist on disk (cloning creates the directory right away) get the status
    /// overlaid; the rest get a synthetic placeholder entry
    fn workspace_repos_with_pending(&self) -> Vec<RepoInfo> {
        let mut repos = self.workspace_repos_list.clone();
        for pending in &self.pending_clones {
            if let Some(repo) = repos
                .iter_mut()
                .find(|r| r.display_name == pending.display_name)
            {
                repo.operation_status = pending.status.clone();
            } else {
                repos.push(RepoInfo {
                    path: PathBuf::from(format!(
                        "{}/{}",
                        self.workspace_path, pending.display_name
                    )),
                    display_name: pending.display_name.clone(),
                    is_clean: true,
                    modification_time: None,
                    size_bytes: None,
                    operation_status: pending.status.clone(),
                    is_submodule: false,
                    submodule_initialized: false,
                    parent_repo_path: None,
                });
            }
        }
        repos
    }

    /// Rebuild the workspace tree (and filtered views) after the pending
    /// clones changed, preserving a user-made selection
    fn rebuild_after_pending_change(&mut self) {
        let previous = if self.selection_is_auto {
            None
        } else {
            self.selected_position()
        };

        self.workspace_tree = build_tree(self.workspace_repos_with_pending());
        self.rebuild_filtered();

        match previous {
            Some((section, index, path)) => self.restore_selection(section, index, &path),
            None => self.select_first_available(),
        }
    }

    /// Show a temporary "cloning..." row for the given repo pattern
    pub fn add_pending_clone(&mut self, display_name: String) {
        self.pending_clones
            .retain(|p| p.display_name != display_name);
        self.pending_clones.push(PendingClone {
            display_name,
            status: RepoOperationStatus::Cloning,
            failed_at: None,
        });
        self.rebuild_after_pending_change();
    }

    /// Resolve a pending clone: drop the row on success (the refresh brings in
    /// the real repo), or mark it failed so the error shows on the row
    pub fn finish_pending_clone(&mut self, display_name: &str, error: Option<String>) {
        match error {
            None => self
                .pending_clones
                .retain(|p| p.display_name != display_name),
            Some(err) => {
                if let Some(pending) = self
                    .pending_clones
                    .iter_mut()
                    .find(|p| p.display_name == display_name)
                {
                    pending.status = RepoOperationStatus::Failed(err);
                    pending.failed_at = Some(Instant::now());
                }
            }
        }
        self.rebuild_after_pending_change();
    }

    /// Remove failed clone rows older than `ttl` so they don't linger forever
    pub fn expire_failed_clones(&mut self, ttl: Duration) {
        let before = self.pending_clones.len();
        self.pending_clones
            .retain(|p| p.failed_at.is_none_or(|at| at.elapsed() < ttl));
        if self.pending_clones.len() != before {
            self.rebuild_after_pending_change();
        }
    }

    /// Select the first item in the first section that has any, preferring the
    /// workspace. Marks the selection as automatic.
    fn select_first_available(&mut self) {
        if !self.filtered_workspace.is_empty() {
            self.select(Section::Workspace, 0);
        } else if !self.filtered_library.is_empty() {
            self.select(Section::Library, 0);
        } else {
            self.workspace_state.select(None);
            self.library_state.select(None);
        }
        self.selection_is_auto = true;
    }

    pub fn get_flattened_workspace(&self) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(&self.filtered_workspace)
    }

    pub fn get_flattened_library(&self) -> Vec<(&TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(&self.filtered_library)
    }

    pub fn count_workspace_repos(&self) -> usize {
        count_repos_in_trees(&self.filtered_workspace)
    }

    pub fn count_library_repos(&self) -> usize {
        count_repos_in_trees(&self.filtered_library)
    }

    /// Select the given index in the given section, clearing the other section
    fn select(&mut self, section: Section, index: usize) {
        let (target, other) = match section {
            Section::Workspace => (&mut self.workspace_state, &mut self.library_state),
            Section::Library => (&mut self.library_state, &mut self.workspace_state),
        };
        target.select(Some(index));
        other.select(None);
        self.active_section = section;
    }

    /// Number of visible (flattened) items in the given section
    fn section_len(&self, section: Section) -> usize {
        match section {
            Section::Workspace => flatten_trees(&self.filtered_workspace).len(),
            Section::Library => flatten_trees(&self.filtered_library).len(),
        }
    }

    /// Switch to the other section if it has any items
    pub fn switch_section(&mut self) {
        let other = self.active_section.other();
        if self.section_len(other) > 0 {
            self.select(other, 0);
            self.selection_is_auto = false;
        }
    }

    pub fn next(&mut self) {
        self.move_selection(1);
    }

    pub fn previous(&mut self) {
        self.move_selection(-1);
    }

    /// Move the selection by one, crossing into the other section at the edges
    fn move_selection(&mut self, delta: isize) {
        let section = self.active_section;
        let current_len = self.section_len(section);
        if current_len == 0 {
            return;
        }
        self.selection_is_auto = false;

        let selected = match section {
            Section::Workspace => self.workspace_state.selected(),
            Section::Library => self.library_state.selected(),
        };
        let Some(i) = selected else {
            self.select(section, 0);
            return;
        };

        let at_edge = if delta > 0 {
            i + 1 >= current_len
        } else {
            i == 0
        };
        if !at_edge {
            self.select(section, i.saturating_add_signed(delta));
        } else if self.section_len(section.other()) > 0 {
            // Cross into the other section (top when moving down, bottom when moving up)
            let other_len = self.section_len(section.other());
            let index = if delta > 0 { 0 } else { other_len - 1 };
            self.select(section.other(), index);
        } else {
            // Wrap within the current section
            let index = if delta > 0 { 0 } else { current_len - 1 };
            self.select(section, index);
        }
    }

    /// The currently selected tree node in the active section
    pub fn selected_node(&self) -> Option<&TreeNode> {
        let (trees, state) = match self.active_section {
            Section::Workspace => (&self.filtered_workspace, &self.workspace_state),
            Section::Library => (&self.filtered_library, &self.library_state),
        };
        let index = state.selected()?;
        flatten_trees(trees).get(index).map(|(node, _, _, _)| *node)
    }

    pub fn toggle_expand(&mut self) {
        let (trees, state) = match self.active_section {
            Section::Workspace => (&self.filtered_workspace, &self.workspace_state),
            Section::Library => (&self.filtered_library, &self.library_state),
        };
        let index_path = state.selected().and_then(|i| {
            flatten_trees(trees)
                .get(i)
                .map(|(_, _, path, _)| path.clone())
        });

        if let Some(index_path) = index_path {
            let trees = match self.active_section {
                Section::Workspace => &mut self.filtered_workspace,
                Section::Library => &mut self.filtered_library,
            };
            toggle_node_at_path(trees, &index_path);
        }
    }

    /// Clone-dialog suggestions filtered by the current input
    pub fn filtered_suggestions(&self) -> Vec<&str> {
        let input = self.clone_repo_input.to_lowercase();
        self.clone_repo_suggestions
            .iter()
            .filter(|s| s.to_lowercase().contains(&input))
            .map(|s| s.as_str())
            .collect()
    }

    pub fn update_repo_status(
        &mut self,
        display_name: &str,
        status: super::tree::RepoOperationStatus,
    ) {
        // Update in workspace tree
        update_repo_status_in_tree(&mut self.workspace_tree, display_name, status.clone());
        update_repo_status_in_tree(&mut self.filtered_workspace, display_name, status.clone());

        // Update in library tree
        update_repo_status_in_tree(&mut self.library_tree, display_name, status.clone());
        update_repo_status_in_tree(&mut self.filtered_library, display_name, status);
    }

    /// Update the app with new repository data (for real-time loading).
    /// A selection made by the user is preserved across the update; automatic
    /// selections are redone so the workspace is preferred once it has repos.
    pub fn update_repos(&mut self, workspace_repos: Vec<RepoInfo>, library_repos: Vec<RepoInfo>) {
        let previous = if self.selection_is_auto {
            None
        } else {
            self.selected_position()
        };

        self.library_tree = build_library_tree(library_repos.clone(), &workspace_repos);
        self.workspace_repos_list = workspace_repos;
        self.library_repos_list = library_repos;
        self.workspace_tree = build_tree(self.workspace_repos_with_pending());
        self.rebuild_filtered();

        match previous {
            Some((section, index, path)) => self.restore_selection(section, index, &path),
            None => self.select_first_available(),
        }
    }

    /// The section, index, and full path of the currently selected item
    fn selected_position(&self) -> Option<(Section, usize, String)> {
        let (trees, state) = match self.active_section {
            Section::Workspace => (&self.filtered_workspace, &self.workspace_state),
            Section::Library => (&self.filtered_library, &self.library_state),
        };
        let index = state.selected()?;
        let path = flatten_trees(trees)
            .get(index)
            .map(|(_, _, _, path)| path.clone())?;
        Some((self.active_section, index, path))
    }

    /// Re-select the item with the given full path, checking both sections so
    /// the selection follows a repo that was dropped or restored. Falls back to
    /// the nearest index in the previous section.
    fn restore_selection(&mut self, prev_section: Section, prev_index: usize, path: &str) {
        for section in [prev_section, prev_section.other()] {
            let trees = match section {
                Section::Workspace => &self.filtered_workspace,
                Section::Library => &self.filtered_library,
            };
            if let Some(index) = flatten_trees(trees)
                .iter()
                .position(|(_, _, _, p)| p == path)
            {
                self.select(section, index);
                return;
            }
        }

        let len = self.section_len(prev_section);
        if len > 0 {
            self.select(prev_section, prev_index.min(len - 1));
        } else {
            self.select_first_available();
        }
    }
}

fn update_repo_status_in_tree(
    nodes: &mut [TreeNode],
    display_name: &str,
    status: super::tree::RepoOperationStatus,
) {
    for node in nodes {
        if let Some(ref mut repo) = node.repo_info
            && repo.display_name == display_name
        {
            repo.operation_status = status.clone();
        }
        update_repo_status_in_tree(&mut node.children, display_name, status.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(display_name: &str) -> RepoInfo {
        RepoInfo {
            path: PathBuf::from(display_name),
            display_name: display_name.to_string(),
            is_clean: true,
            modification_time: None,
            size_bytes: None,
            operation_status: super::super::tree::RepoOperationStatus::None,
            is_submodule: false,
            submodule_initialized: false,
            parent_repo_path: None,
        }
    }

    fn selected_path(app: &App) -> Option<String> {
        app.selected_position().map(|(_, _, path)| path)
    }

    fn visible_workspace_paths(app: &App) -> Vec<String> {
        flatten_trees(&app.filtered_workspace)
            .iter()
            .map(|(_, _, _, p)| p.clone())
            .collect()
    }

    #[test]
    fn pending_clone_appears_and_resolves() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            Vec::new(),
        );

        app.add_pending_clone("github.com/nosuch/thing".to_string());
        let names = visible_workspace_paths(&app);
        assert!(
            names.contains(&"github.com/nosuch/thing".to_string()),
            "placeholder missing: {:?}",
            names
        );

        // Cloning creates the directory immediately, so a rescan finds a real
        // repo mid-clone; the cloning status must stay on the row
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/nosuch/thing")],
            Vec::new(),
        );
        let status = flatten_trees(&app.filtered_workspace)
            .iter()
            .find(|(_, _, _, p)| p == "github.com/nosuch/thing")
            .and_then(|(node, _, _, _)| node.repo_info.as_ref())
            .map(|r| r.operation_status.clone());
        assert!(matches!(status, Some(RepoOperationStatus::Cloning)));

        // Failure keeps the row so the error is visible
        app.finish_pending_clone("github.com/nosuch/thing", Some("boom".to_string()));
        assert!(visible_workspace_paths(&app).contains(&"github.com/nosuch/thing".to_string()));

        // Expiry removes the overlay (the row survives here because the repo
        // landed on disk above)
        app.expire_failed_clones(Duration::from_secs(0));
        let status = flatten_trees(&app.filtered_workspace)
            .iter()
            .find(|(_, _, _, p)| p == "github.com/nosuch/thing")
            .and_then(|(node, _, _, _)| node.repo_info.as_ref())
            .map(|r| r.operation_status.clone());
        assert!(matches!(status, Some(RepoOperationStatus::None)));
    }

    #[test]
    fn auto_selection_prefers_workspace_once_it_loads() {
        // Library results arrive first (parallel loading is unordered)
        let mut app = App::new(
            "workspace".to_string(),
            Vec::new(),
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Library);

        // Workspace results arrive later; the automatic selection moves over
        app.update_repos(
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Workspace);

        // Further streaming updates keep it in the workspace
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/other")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Workspace);
    }

    #[test]
    fn user_selection_survives_streaming_updates() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );

        // User moves into the library
        app.switch_section();
        assert_eq!(app.active_section, Section::Library);
        let path = selected_path(&app).unwrap();

        // More workspace repos stream in; the selection must not jump back
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/foo/other")],
            vec![repo("github.com/bar/lib")],
        );
        assert_eq!(app.active_section, Section::Library);
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn selection_follows_item_when_siblings_shift() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app"), repo("github.com/foo/zeta")],
            Vec::new(),
        );

        // Move down to a specific repo
        app.next();
        app.next();
        let path = selected_path(&app).unwrap();

        // A new repo is inserted above it in the tree
        app.update_repos(
            vec![
                repo("github.com/aaa/first"),
                repo("github.com/foo/app"),
                repo("github.com/foo/zeta"),
            ],
            Vec::new(),
        );
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn selection_follows_repo_across_sections() {
        let mut app = App::new(
            "workspace".to_string(),
            vec![repo("github.com/foo/app")],
            vec![repo("github.com/bar/lib")],
        );

        // User selects the library repo (a leaf, two levels deep)
        app.switch_section();
        app.next();
        app.next();
        let path = selected_path(&app).unwrap();
        assert_eq!(path, "github.com/bar/lib");

        // The repo is restored into the workspace
        app.update_repos(
            vec![repo("github.com/foo/app"), repo("github.com/bar/lib")],
            Vec::new(),
        );
        assert_eq!(app.active_section, Section::Workspace);
        assert_eq!(selected_path(&app).as_deref(), Some(path.as_str()));
    }
}
