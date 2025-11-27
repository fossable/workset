use super::tree::{
    RepoInfo, TreeNode, TreeState, build_library_tree, build_tree, count_repos_in_trees,
    flatten_trees, toggle_node_at_path,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

#[derive(PartialEq)]
pub enum AppMode {
    Normal,
    AddRepo,
}

#[derive(PartialEq)]
pub enum Section {
    Workspace,
    Library,
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
    pub add_repo_input: String,
    pub add_repo_suggestions: Vec<String>,
    pub add_repo_state: TreeState,
}

impl App {
    pub fn new(workspace_repos: Vec<RepoInfo>, library_repos: Vec<RepoInfo>) -> Self {
        let workspace_tree = build_tree(workspace_repos.clone());
        let library_tree = build_library_tree(library_repos.clone(), &workspace_repos);

        let filtered_workspace = workspace_tree.clone();
        let filtered_library = library_tree.clone();

        let mut workspace_state = TreeState::new();
        let mut library_state = TreeState::new();

        // Select first item in whichever section has items
        let active_section = if !workspace_tree.is_empty() {
            workspace_state.select(Some(0));
            Section::Workspace
        } else if !library_tree.is_empty() {
            library_state.select(Some(0));
            Section::Library
        } else {
            Section::Workspace
        };

        Self {
            workspace_tree,
            library_tree,
            workspace_repos_list: workspace_repos,
            library_repos_list: library_repos,
            filtered_workspace,
            filtered_library,
            workspace_state,
            library_state,
            search_query: String::new(),
            active_section,
            matcher: SkimMatcherV2::default(),
            last_log_message: String::new(),
            mode: AppMode::Normal,
            add_repo_input: String::new(),
            add_repo_suggestions: Vec::new(),
            add_repo_state: TreeState::new(),
        }
    }

    pub fn filter_repos(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_workspace = self.workspace_tree.clone();
            self.filtered_library = self.library_tree.clone();
        } else {
            // Filter workspace repos by search query
            let filtered_workspace_repos: Vec<RepoInfo> = self
                .workspace_repos_list
                .iter()
                .filter(|r| {
                    self.matcher
                        .fuzzy_match(&r.display_name, &self.search_query)
                        .is_some()
                })
                .cloned()
                .collect();
            self.filtered_workspace = build_tree(filtered_workspace_repos);

            // Filter library repos by search query
            let filtered_library_repos: Vec<RepoInfo> = self
                .library_repos_list
                .iter()
                .filter(|r| {
                    self.matcher
                        .fuzzy_match(&r.display_name, &self.search_query)
                        .is_some()
                })
                .cloned()
                .collect();
            self.filtered_library =
                build_library_tree(filtered_library_repos, &self.workspace_repos_list);
        }

        // Flatten to get item counts
        let workspace_flat = flatten_trees(&self.filtered_workspace);
        let library_flat = flatten_trees(&self.filtered_library);

        // Reset selection
        if !workspace_flat.is_empty() {
            self.workspace_state.select(Some(0));
            self.library_state.select(None);
            self.active_section = Section::Workspace;
        } else if !library_flat.is_empty() {
            self.workspace_state.select(None);
            self.library_state.select(Some(0));
            self.active_section = Section::Library;
        } else {
            self.workspace_state.select(None);
            self.library_state.select(None);
        }
    }

    pub fn get_flattened_workspace(&self) -> Vec<(TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(&self.filtered_workspace)
    }

    pub fn get_flattened_library(&self) -> Vec<(TreeNode, usize, Vec<usize>, String)> {
        flatten_trees(&self.filtered_library)
    }

    pub fn count_workspace_repos(&self) -> usize {
        count_repos_in_trees(&self.filtered_workspace)
    }

    pub fn count_library_repos(&self) -> usize {
        count_repos_in_trees(&self.filtered_library)
    }

    pub fn next(&mut self) {
        match self.active_section {
            Section::Workspace => {
                let workspace_items = self.get_flattened_workspace();
                if workspace_items.is_empty() {
                    return;
                }
                let i = match self.workspace_state.selected() {
                    Some(i) => {
                        if i >= workspace_items.len() - 1 {
                            // Move to library section if available
                            let library_items = self.get_flattened_library();
                            if !library_items.is_empty() {
                                self.workspace_state.select(None);
                                self.library_state.select(Some(0));
                                self.active_section = Section::Library;
                                return;
                            }
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.workspace_state.select(Some(i));
            }
            Section::Library => {
                let library_items = self.get_flattened_library();
                if library_items.is_empty() {
                    return;
                }
                let i = match self.library_state.selected() {
                    Some(i) => {
                        if i >= library_items.len() - 1 {
                            // Wrap to workspace section if available
                            let workspace_items = self.get_flattened_workspace();
                            if !workspace_items.is_empty() {
                                self.library_state.select(None);
                                self.workspace_state.select(Some(0));
                                self.active_section = Section::Workspace;
                                return;
                            }
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.library_state.select(Some(i));
            }
        }
    }

    pub fn previous(&mut self) {
        match self.active_section {
            Section::Workspace => {
                let workspace_items = self.get_flattened_workspace();
                if workspace_items.is_empty() {
                    return;
                }
                let i = match self.workspace_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            // Move to library section if available
                            let library_items = self.get_flattened_library();
                            if !library_items.is_empty() {
                                self.workspace_state.select(None);
                                self.library_state.select(Some(library_items.len() - 1));
                                self.active_section = Section::Library;
                                return;
                            }
                            workspace_items.len() - 1
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.workspace_state.select(Some(i));
            }
            Section::Library => {
                let library_items = self.get_flattened_library();
                if library_items.is_empty() {
                    return;
                }
                let i = match self.library_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            // Wrap to workspace section if available
                            let workspace_items = self.get_flattened_workspace();
                            if !workspace_items.is_empty() {
                                self.library_state.select(None);
                                self.workspace_state.select(Some(workspace_items.len() - 1));
                                self.active_section = Section::Workspace;
                                return;
                            }
                            library_items.len() - 1
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.library_state.select(Some(i));
            }
        }
    }

    pub fn selected_workspace_node(&self) -> Option<TreeNode> {
        let items = self.get_flattened_workspace();
        self.workspace_state
            .selected()
            .and_then(|i| items.get(i).map(|(node, _, _, _)| node.clone()))
    }

    pub fn selected_library_node(&self) -> Option<TreeNode> {
        let items = self.get_flattened_library();
        self.library_state
            .selected()
            .and_then(|i| items.get(i).map(|(node, _, _, _)| node.clone()))
    }

    pub fn toggle_expand(&mut self) {
        if self.active_section == Section::Workspace {
            let items = self.get_flattened_workspace();
            if let Some(selected_idx) = self.workspace_state.selected()
                && let Some((_, _, index_path, _)) = items.get(selected_idx).cloned()
            {
                toggle_node_at_path(&mut self.filtered_workspace, &index_path);
            }
        } else {
            let items = self.get_flattened_library();
            if let Some(selected_idx) = self.library_state.selected()
                && let Some((_, _, index_path, _)) = items.get(selected_idx).cloned()
            {
                toggle_node_at_path(&mut self.filtered_library, &index_path);
            }
        }
    }

    pub fn update_repo_status(&mut self, display_name: &str, status: super::tree::RepoOperationStatus) {
        // Update in workspace tree
        update_repo_status_in_tree(&mut self.workspace_tree, display_name, status.clone());
        update_repo_status_in_tree(&mut self.filtered_workspace, display_name, status.clone());

        // Update in library tree
        update_repo_status_in_tree(&mut self.library_tree, display_name, status.clone());
        update_repo_status_in_tree(&mut self.filtered_library, display_name, status);
    }

}

fn update_repo_status_in_tree(nodes: &mut [TreeNode], display_name: &str, status: super::tree::RepoOperationStatus) {
    for node in nodes {
        if let Some(ref mut repo) = node.repo_info {
            if repo.display_name == display_name {
                repo.operation_status = status.clone();
            }
        }
        update_repo_status_in_tree(&mut node.children, display_name, status.clone());
    }
}
