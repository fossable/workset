use super::tree::{
    RepoInfo, TreeNode, TreeState, build_library_tree, build_tree, count_repos_in_trees,
    flatten_trees, toggle_node_at_path,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    CloneRepo,
}

#[derive(PartialEq, Clone, Copy)]
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
    pub last_log_message: String,
    pub mode: AppMode,
    pub clone_repo_input: String,
    pub clone_repo_suggestions: Vec<String>,
    pub clone_repo_state: TreeState,
}

impl App {
    pub fn new(workspace_repos: Vec<RepoInfo>, library_repos: Vec<RepoInfo>) -> Self {
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
            last_log_message: String::new(),
            mode: AppMode::Normal,
            clone_repo_input: String::new(),
            clone_repo_suggestions: Vec::new(),
            clone_repo_state: TreeState::new(),
        };
        app.update_repos(workspace_repos, library_repos);
        app
    }

    pub fn filter_repos(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_workspace = self.workspace_tree.clone();
            self.filtered_library = self.library_tree.clone();
        } else {
            // Filter repos by fuzzy-matching the search query
            let matches = |r: &&RepoInfo| {
                self.matcher
                    .fuzzy_match(&r.display_name, &self.search_query)
                    .is_some()
            };
            self.filtered_workspace = build_tree(
                self.workspace_repos_list
                    .iter()
                    .filter(matches)
                    .cloned()
                    .collect(),
            );
            self.filtered_library = build_library_tree(
                self.library_repos_list
                    .iter()
                    .filter(matches)
                    .cloned()
                    .collect(),
                &self.workspace_repos_list,
            );
        }

        // Reset selection to the first section with items
        if !self.filtered_workspace.is_empty() {
            self.select(Section::Workspace, 0);
        } else if !self.filtered_library.is_empty() {
            self.select(Section::Library, 0);
        } else {
            self.workspace_state.select(None);
            self.library_state.select(None);
        }
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

    /// Update the app with new repository data (for real-time loading)
    pub fn update_repos(&mut self, workspace_repos: Vec<RepoInfo>, library_repos: Vec<RepoInfo>) {
        let workspace_tree = build_tree(workspace_repos.clone());
        let library_tree = build_library_tree(library_repos.clone(), &workspace_repos);

        self.workspace_tree = workspace_tree.clone();
        self.library_tree = library_tree.clone();
        self.workspace_repos_list = workspace_repos;
        self.library_repos_list = library_repos;
        self.filtered_workspace = workspace_tree;
        self.filtered_library = library_tree;

        // Ensure something is selected if we have repos
        if self.workspace_state.selected().is_none() && !self.workspace_tree.is_empty() {
            self.select(Section::Workspace, 0);
        } else if self.library_state.selected().is_none() && !self.library_tree.is_empty() {
            self.select(Section::Library, 0);
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
