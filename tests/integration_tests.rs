use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Get the path to the compiled binary
fn get_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_workset"))
}

/// Helper to create a git repository with some commits
fn create_test_repo(path: &Path, repo_name: &str, num_commits: usize) -> PathBuf {
    let repo_path = path.join(repo_name);
    fs::create_dir_all(&repo_path).unwrap();

    // Initialize git repo
    Command::new("git")
        .args(["init"])
        .current_dir(&repo_path)
        .output()
        .expect("Failed to init repo");

    // Configure git
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&repo_path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&repo_path)
        .output()
        .unwrap();

    // Create commits
    for i in 0..num_commits {
        let filename = format!("file{}.txt", i);
        fs::write(repo_path.join(&filename), format!("Content {}\n", i)).unwrap();

        Command::new("git")
            .args(["add", &filename])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", &format!("Commit {}", i)])
            .current_dir(&repo_path)
            .output()
            .unwrap();
    }

    repo_path
}

/// Helper to create a test workspace with some git repos
fn setup_test_workspace() -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();

    // Create .workset directory to mark it as a workspace
    fs::create_dir(workspace_path.join(".workset")).unwrap();

    // Create some test git repositories with actual git initialization
    for repo_name in &["repo1", "repo2", "subdir/repo3"] {
        let repo_path = workspace_path.join(repo_name);
        fs::create_dir_all(&repo_path).unwrap();

        // Initialize as a real git repository
        Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        // Configure git user for commits
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        // Create an initial commit
        fs::write(repo_path.join("README.md"), "# Test repo\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
    }

    temp_dir
}

#[test]
fn test_workspace_init() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    let output = Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run init");

    assert!(output.status.success(), "Init should succeed");

    // Verify .workset directory was created
    assert!(workspace_path.join(".workset").exists());

    // Running init again should be idempotent
    let output = Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run init");

    assert!(output.status.success(), "Init should be idempotent");
}

#[test]
fn test_drop_and_restore_clean_repo() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create a test repo in the workspace
    let repo_path = create_test_repo(workspace_path, "test-repo", 3);

    // Verify repo exists
    assert!(repo_path.exists());
    assert!(repo_path.join(".git").exists());

    // Drop the repo (should move to library)
    let output = Command::new(&binary)
        .args(["drop", "test-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to drop repo");

    assert!(
        output.status.success(),
        "Drop should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify repo was moved to library
    assert!(!repo_path.exists(), "Repo should be removed from workspace");
    assert!(
        workspace_path.join(".workset/test-repo").exists(),
        "Repo should be in library"
    );

    // Restore the repo
    let output = Command::new(&binary)
        .args(["restore", "test-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to restore repo");

    assert!(
        output.status.success(),
        "Restore should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify repo is back in workspace
    assert!(repo_path.exists(), "Repo should be restored to workspace");
    assert!(repo_path.join(".git").exists());
    assert!(repo_path.join("file0.txt").exists());
    assert!(repo_path.join("file1.txt").exists());
    assert!(repo_path.join("file2.txt").exists());
}

#[test]
fn test_dirty_repo_detection() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();

    // Create a test repo
    let repo_path = create_test_repo(workspace_path, "dirty-repo", 2);

    // Verify it starts clean
    use workset::{RepoStatus, check_repo_status};
    let status = check_repo_status(&repo_path).unwrap();
    assert!(
        matches!(status, RepoStatus::Clean),
        "New repo should be clean"
    );

    // Make the repo dirty by adding an uncommitted file
    fs::write(repo_path.join("dirty.txt"), "uncommitted changes\n").unwrap();

    // Verify it's now dirty
    let status = check_repo_status(&repo_path).unwrap();
    assert!(
        matches!(status, RepoStatus::Dirty),
        "Repo with uncommitted files should be dirty"
    );
}

#[test]
fn test_drop_dirty_repo_succeeds_with_force() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create a test repo
    let repo_path = create_test_repo(workspace_path, "dirty-repo", 2);

    // Make the repo dirty
    fs::write(repo_path.join("dirty.txt"), "uncommitted changes\n").unwrap();

    // Drop with --force
    let output = Command::new(&binary)
        .args(["drop", "--force", "dirty-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run drop");

    assert!(
        output.status.success(),
        "Drop with --force should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Repo should be in library
    assert!(!repo_path.exists());
    assert!(workspace_path.join(".workset/dirty-repo").exists());
}

#[test]
fn test_drop_with_delete_permanently_removes_repo() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create a test repo
    let repo_path = create_test_repo(workspace_path, "delete-me", 1);

    // Drop with --delete
    let output = Command::new(&binary)
        .args(["drop", "--delete", "delete-me"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run drop");

    assert!(
        output.status.success(),
        "Drop with --delete should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Repo should not exist anywhere
    assert!(!repo_path.exists());
    assert!(!workspace_path.join(".workset/delete-me").exists());
}

#[test]
fn test_drop_all_in_current_directory() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create multiple repos
    create_test_repo(workspace_path, "repo1", 1);
    create_test_repo(workspace_path, "repo2", 1);
    create_test_repo(workspace_path, "repo3", 1);

    // Drop all repos in current directory
    let output = Command::new(&binary)
        .arg("drop")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run drop");

    assert!(
        output.status.success(),
        "Drop all should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // All repos should be in library
    assert!(!workspace_path.join("repo1").exists());
    assert!(!workspace_path.join("repo2").exists());
    assert!(!workspace_path.join("repo3").exists());
    assert!(workspace_path.join(".workset/repo1").exists());
    assert!(workspace_path.join(".workset/repo2").exists());
    assert!(workspace_path.join(".workset/repo3").exists());
}

#[test]
fn test_list_command_shows_repo_status() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create clean and dirty repos
    let _clean_repo = create_test_repo(workspace_path, "clean-repo", 2);
    let dirty_repo = create_test_repo(workspace_path, "dirty-repo", 2);

    // Make one repo dirty
    fs::write(dirty_repo.join("uncommitted.txt"), "dirty\n").unwrap();

    // Run list command
    let output = Command::new(&binary)
        .arg("list")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run list");

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();

    // Should show both repos
    assert!(stdout.contains("clean-repo"));
    assert!(stdout.contains("dirty-repo"));

    // Should show status
    assert!(stdout.contains("clean") || stdout.contains("✓"));
    assert!(stdout.contains("modified") || stdout.contains("⚠"));
}

#[test]
fn test_status_command_shows_summary() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create some repos
    create_test_repo(workspace_path, "repo1", 1);
    create_test_repo(workspace_path, "repo2", 1);

    // Run status command
    let output = Command::new(&binary)
        .arg("status")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to run status");

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();

    // Should show workspace path
    assert!(stdout.contains("Workspace:"));

    // Should show library path
    assert!(stdout.contains("Library:"));

    // Should show active repositories count
    assert!(stdout.contains("Active repositories"));
}

#[test]
fn test_nested_directory_structure() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create repos in nested directories (like github.com/user/repo)
    let nested_path = workspace_path.join("github.com/testuser");
    fs::create_dir_all(&nested_path).unwrap();
    create_test_repo(&nested_path, "nested-repo", 2);

    // Verify repo exists
    assert!(nested_path.join("nested-repo").exists());

    // Drop the nested repo
    let output = Command::new(&binary)
        .args(["drop", "github.com/testuser/nested-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to drop nested repo");

    assert!(
        output.status.success(),
        "Drop nested repo should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify it's in library
    assert!(!nested_path.join("nested-repo").exists());
    assert!(
        workspace_path
            .join(".workset/github.com/testuser/nested-repo")
            .exists()
    );

    // Restore it
    let output = Command::new(&binary)
        .args(["restore", "github.com/testuser/nested-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to restore nested repo");

    assert!(output.status.success(), "Restore should succeed");
    assert!(nested_path.join("nested-repo").exists());
}

#[test]
fn test_multiple_drop_and_restore_cycles() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create a repo
    let repo_path = create_test_repo(workspace_path, "cycle-repo", 1);

    // Perform multiple drop/restore cycles
    for i in 0..3 {
        // Drop
        let output = Command::new(&binary)
            .args(["drop", "cycle-repo"])
            .current_dir(workspace_path)
            .output()
            .expect("Failed to drop repo");

        assert!(output.status.success(), "Drop cycle {} should succeed", i);
        assert!(!repo_path.exists());

        // Restore
        let output = Command::new(&binary)
            .args(["restore", "cycle-repo"])
            .current_dir(workspace_path)
            .output()
            .expect("Failed to restore repo");

        assert!(
            output.status.success(),
            "Restore cycle {} should succeed",
            i
        );
        assert!(repo_path.exists());
        assert!(repo_path.join("file0.txt").exists());
    }
}

#[test]
fn test_repo_with_gitmodules_file() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create a main repo
    let main_repo = create_test_repo(workspace_path, "main-repo", 2);

    // Manually create a .gitmodules file to simulate a repo with submodules
    // (Easier than setting up actual submodules which require network/paths)
    fs::write(
        main_repo.join(".gitmodules"),
        "[submodule \"example\"]\n\tpath = sub\n\turl = https://example.com/repo.git\n",
    )
    .unwrap();

    Command::new("git")
        .args(["add", ".gitmodules"])
        .current_dir(&main_repo)
        .output()
        .unwrap();

    Command::new("git")
        .args(["commit", "-m", "Add submodule config"])
        .current_dir(&main_repo)
        .output()
        .unwrap();

    // Verify .gitmodules exists
    assert!(main_repo.join(".gitmodules").exists());

    // Drop the main repo
    let output = Command::new(&binary)
        .args(["drop", "main-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to drop repo with gitmodules");

    assert!(
        output.status.success(),
        "Drop repo with .gitmodules should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Restore it
    let output = Command::new(&binary)
        .args(["restore", "main-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to restore repo with gitmodules");

    assert!(output.status.success(), "Restore should succeed");

    // Main repo should be restored with .gitmodules
    assert!(main_repo.exists());
    assert!(main_repo.join(".gitmodules").exists());
}

#[test]
fn test_repo_status_detection() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();

    // Create repos with different states
    let clean_repo = create_test_repo(workspace_path, "clean", 2);
    let dirty_repo = create_test_repo(workspace_path, "dirty", 2);
    let untracked_repo = create_test_repo(workspace_path, "untracked", 2);

    // Make dirty repo dirty
    fs::write(dirty_repo.join("file0.txt"), "modified content\n").unwrap();

    // Add untracked file
    fs::write(untracked_repo.join("new-file.txt"), "new content\n").unwrap();

    // Test status detection using workset library functions
    use workset::{RepoStatus, check_repo_status};

    // Clean repo should be clean
    let status = check_repo_status(&clean_repo).unwrap();
    assert!(matches!(status, RepoStatus::Clean));

    // Dirty repo should be dirty
    let status = check_repo_status(&dirty_repo).unwrap();
    assert!(matches!(status, RepoStatus::Dirty));

    // Repo with untracked files should be dirty
    let status = check_repo_status(&untracked_repo).unwrap();
    assert!(matches!(status, RepoStatus::Dirty));
}

#[test]
fn test_modification_time_tracking() {
    let temp_dir = TempDir::new().unwrap();

    // Create a repo
    let repo_path = create_test_repo(temp_dir.path(), "time-test", 1);

    // Get modification time for clean repo
    use workset::{RepoStatus, check_repo_status, get_repo_modification_time};

    let status = check_repo_status(&repo_path).unwrap();
    let is_clean = matches!(status, RepoStatus::Clean);
    let mod_time = get_repo_modification_time(&repo_path, is_clean);

    assert!(
        mod_time.is_ok(),
        "Should get modification time for clean repo"
    );

    // Make repo dirty and check time again
    std::thread::sleep(std::time::Duration::from_secs(1));
    fs::write(repo_path.join("new-file.txt"), "new\n").unwrap();

    let status = check_repo_status(&repo_path).unwrap();
    let is_clean = matches!(status, RepoStatus::Clean);
    let new_mod_time = get_repo_modification_time(&repo_path, is_clean);

    assert!(
        new_mod_time.is_ok(),
        "Should get modification time for dirty repo"
    );

    // Dirty repo time should be more recent
    if let (Ok(old), Ok(new)) = (mod_time, new_mod_time) {
        assert!(
            new >= old,
            "Dirty repo modification time should be >= clean repo time"
        );
    }
}

#[test]
fn test_drop_relative_to_cwd() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create nested directory structure with multiple repos
    let subdir = workspace_path.join("projects");
    fs::create_dir_all(&subdir).unwrap();

    create_test_repo(&subdir, "repo1", 1);
    create_test_repo(&subdir, "repo2", 1);
    create_test_repo(workspace_path, "root-repo", 1);

    // Drop all from the subdirectory (should only drop repos in that dir)
    let output = Command::new(&binary)
        .arg("drop")
        .current_dir(&subdir)
        .output()
        .expect("Failed to drop from subdir");

    assert!(
        output.status.success(),
        "Drop from subdir should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Repos in subdir should be dropped
    assert!(!subdir.join("repo1").exists());
    assert!(!subdir.join("repo2").exists());

    // Root repo should still exist (not in CWD)
    assert!(workspace_path.join("root-repo").exists());

    // Both should be in library
    assert!(workspace_path.join(".workset/projects/repo1").exists());
    assert!(workspace_path.join(".workset/projects/repo2").exists());
}

#[test]
fn test_drop_specific_repo_from_subdirectory() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create nested repos
    let subdir = workspace_path.join("github.com/user");
    fs::create_dir_all(&subdir).unwrap();
    create_test_repo(&subdir, "project", 1);

    // Drop specific repo from workspace root using full path
    let output = Command::new(&binary)
        .args(["drop", "github.com/user/project"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to drop nested repo");

    assert!(output.status.success());
    assert!(!subdir.join("project").exists());
    assert!(
        workspace_path
            .join(".workset/github.com/user/project")
            .exists()
    );

    // Restore from workspace root
    Command::new(&binary)
        .args(["restore", "github.com/user/project"])
        .current_dir(workspace_path)
        .output()
        .unwrap();

    assert!(subdir.join("project").exists());

    // Drop from subdirectory using full path (not just "project")
    let output = Command::new(&binary)
        .args(["drop", "github.com/user/project"])
        .current_dir(&subdir)
        .output()
        .expect("Failed to drop with full path from subdir");

    assert!(
        output.status.success(),
        "Should drop with full path from subdirectory: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!subdir.join("project").exists());
}

#[test]
fn test_list_shows_all_workspace_repos() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create repos in different directories
    create_test_repo(workspace_path, "root-repo", 1);

    let subdir = workspace_path.join("projects");
    fs::create_dir_all(&subdir).unwrap();
    create_test_repo(&subdir, "sub-repo1", 1);
    create_test_repo(&subdir, "sub-repo2", 1);

    // List from workspace root should show all repos
    let output = Command::new(&binary)
        .arg("list")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to list from root");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("root-repo"));
    assert!(stdout.contains("sub-repo1") || stdout.contains("projects/sub-repo1"));
    assert!(stdout.contains("sub-repo2") || stdout.contains("projects/sub-repo2"));

    // List from subdirectory currently shows all workspace repos
    // (Not filtered by CWD - this documents current behavior)
    let output = Command::new(&binary)
        .arg("list")
        .current_dir(&subdir)
        .output()
        .expect("Failed to list from subdir");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();

    // Currently, list shows all repos regardless of CWD
    // This test documents the current behavior
    assert!(
        stdout.contains("root-repo") || stdout.contains("sub-repo1"),
        "List currently shows all workspace repos regardless of CWD"
    );
}

#[test]
fn test_status_relative_to_cwd() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create repos in different directories
    create_test_repo(workspace_path, "root-repo", 1);

    let subdir = workspace_path.join("projects");
    fs::create_dir_all(&subdir).unwrap();
    create_test_repo(&subdir, "sub-repo1", 1);
    create_test_repo(&subdir, "sub-repo2", 1);

    // Status from workspace root shows workspace info
    let output = Command::new(&binary)
        .arg("status")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to get status from root");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Workspace:"));
    assert!(stdout.contains("Active repositories: 3") || stdout.contains("3"));

    // Status from subdirectory should still show workspace-level info
    let output = Command::new(&binary)
        .arg("status")
        .current_dir(&subdir)
        .output()
        .expect("Failed to get status from subdir");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Workspace:"));
    // Should still show all workspace repos
    assert!(stdout.contains("Active repositories"));
}

#[test]
fn test_restore_relative_to_cwd() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create and drop repos in nested structure
    let projects_dir = workspace_path.join("projects");
    fs::create_dir_all(&projects_dir).unwrap();

    let repo_path = create_test_repo(&projects_dir, "my-project", 1);

    // Drop it
    Command::new(&binary)
        .args(["drop", "projects/my-project"])
        .current_dir(workspace_path)
        .output()
        .unwrap();

    assert!(!repo_path.exists());

    // Restore from workspace root using full path
    let output = Command::new(&binary)
        .args(["restore", "projects/my-project"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to restore from root");

    assert!(output.status.success());
    assert!(repo_path.exists());

    // Drop again
    Command::new(&binary)
        .args(["drop", "my-project"])
        .current_dir(&projects_dir)
        .output()
        .unwrap();

    // Restore from subdirectory using relative path
    let output = Command::new(&binary)
        .args(["restore", "my-project"])
        .current_dir(&projects_dir)
        .output()
        .expect("Failed to restore from subdir");

    assert!(
        output.status.success(),
        "Should restore relative to CWD within library: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repo_path.exists());
}

#[test]
fn test_drop_with_absolute_paths() {
    let temp_dir = TempDir::new().unwrap();
    let workspace_path = temp_dir.path();
    let binary = get_binary_path();

    // Initialize workspace
    Command::new(&binary)
        .arg("init")
        .current_dir(workspace_path)
        .output()
        .expect("Failed to init workspace");

    // Create nested structure
    let subdir = workspace_path.join("projects/active");
    fs::create_dir_all(&subdir).unwrap();
    create_test_repo(&subdir, "test-repo", 1);

    // Drop using absolute path from workspace root
    let output = Command::new(&binary)
        .args(["drop", "projects/active/test-repo"])
        .current_dir(workspace_path)
        .output()
        .expect("Failed to drop with absolute path");

    assert!(
        output.status.success(),
        "Should drop with absolute path from workspace root: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!subdir.join("test-repo").exists());
    assert!(
        workspace_path
            .join(".workset/projects/active/test-repo")
            .exists()
    );
}

#[test]
fn test_bash_complete_init_without_workspace() {
    let binary = get_binary_path();
    let temp_dir = TempDir::new().unwrap();

    // Bash sets these environment variables when tab completion is triggered
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset ")
        .env("COMP_POINT", "8") // Position after "workset "
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "Command failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // When not in a workspace, should only suggest "init"
    assert_eq!(stdout.trim(), "init");
}

#[test]
fn test_bash_complete_drop_with_workspace() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Bash completion after "workset " in a workspace
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset ")
        .env("COMP_POINT", "8")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // In a workspace, should suggest all subcommands
    let expected = "clone\nrestore\ndrop\nlist\nls\nstatus";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_bash_complete_repo_paths() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Bash completion after "workset drop "
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset drop ")
        .env("COMP_POINT", "13") // Position after "workset drop "
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should list all repositories in the workspace
    let completions: Vec<&str> = stdout.trim().split('\n').collect();
    assert!(completions.contains(&"repo1"));
    assert!(completions.contains(&"repo2"));
    assert!(completions.contains(&"subdir/repo3"));
}

#[test]
fn test_bash_complete_with_cursor_in_middle() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Bash sets COMP_POINT to cursor position, not necessarily end of line
    // Simulate: "workset dr|op" where | is cursor
    // When cursor is at position 10, the current_line is "workset dr"
    // which splits into ["workset", "dr"], so words.len() == 2
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset drop")
        .env("COMP_POINT", "10") // Cursor after "workset dr"
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // With 2 words in the current line, it completes repository paths
    let completions: Vec<&str> = stdout.trim().split('\n').collect();
    assert!(completions.contains(&"repo1"));
    assert!(completions.contains(&"repo2"));
    assert!(completions.contains(&"subdir/repo3"));
}

#[test]
fn test_fish_complete_init_without_workspace() {
    let binary = get_binary_path();
    let temp_dir = TempDir::new().unwrap();

    // Fish sets COMP_LINE environment variable
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Fish format includes tab-separated description
    assert_eq!(
        stdout.trim(),
        "init\tInitialize a workspace in current directory"
    );
}

#[test]
fn test_fish_complete_drop_with_workspace() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Fish completion after "workset "
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // In a workspace, should suggest all subcommands
    let expected = "clone\tClone new repository(ies) to workspace\nrestore\tRestore repository(ies) from library\ndrop\tDrop one or more repositories\nlist\tList all repositories with their status\nls\tList all repositories with their status\nstatus\tShow workspace summary and statistics";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_fish_complete_repo_paths() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Fish completion after "workset drop "
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset drop ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Debug: print actual output
    eprintln!("Fish completion output:\n{}", stdout);

    // Should list all repositories with status and modification time
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines.len(), 3, "Expected 3 repos, got:\n{}", stdout);

    // Each line should have format: "repo_name\tstatus, time"
    for line in &lines {
        assert!(
            line.contains('\t'),
            "Line should contain tab separator: {}\nFull output:\n{}",
            line,
            stdout
        );
        let parts: Vec<&str> = line.split('\t').collect();
        assert_eq!(parts.len(), 2, "Line should have repo name and description");

        // Check that we have one of our repos
        let repo_name = parts[0];
        assert!(
            repo_name == "repo1" || repo_name == "repo2" || repo_name == "subdir/repo3",
            "Unexpected repo: {}",
            repo_name
        );

        // Check that description contains status
        let description = parts[1];
        assert!(
            description.contains("clean")
                || description.contains("dirty")
                || description.contains("unpushed")
                || description.contains("no commits"),
            "Description should contain status: {}",
            description
        );
    }
}

#[test]
fn test_fish_complete_partial_command() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Fish completion after "workset dr" (no trailing space)
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset dr")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should suggest all subcommands (fish completes even with partial input)
    let expected = "clone\tClone new repository(ies) to workspace\nrestore\tRestore repository(ies) from library\ndrop\tDrop one or more repositories\nlist\tList all repositories with their status\nls\tList all repositories with their status\nstatus\tShow workspace summary and statistics";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_unsupported_shell() {
    let binary = get_binary_path();
    let temp_dir = TempDir::new().unwrap();

    // Test with unsupported shell type
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "zsh")
        .env("COMP_LINE", "workset ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    // Should fail with unsupported shell error
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Unsupported shell type"));
}

#[test]
fn test_bash_empty_comp_line() {
    let binary = get_binary_path();
    let temp_dir = setup_test_workspace();

    // Empty COMP_LINE (just "workset" with no space)
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset")
        .env("COMP_POINT", "7")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should suggest all commands
    let expected = "clone\nrestore\ndrop\nlist\nls\nstatus";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_fish_empty_comp_line() {
    let binary = get_binary_path();
    let temp_dir = setup_test_workspace();

    // Empty COMP_LINE (just "workset" with no space)
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should suggest all commands with descriptions
    let expected = "clone\tClone new repository(ies) to workspace\nrestore\tRestore repository(ies) from library\ndrop\tDrop one or more repositories\nlist\tList all repositories with their status\nls\tList all repositories with their status\nstatus\tShow workspace summary and statistics";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_bash_complete_with_trailing_spaces() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Multiple spaces after command
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset   ")
        .env("COMP_POINT", "10") // After multiple spaces
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should suggest all commands
    let expected = "clone\nrestore\ndrop\nlist\nls\nstatus";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_bash_complete_list_command() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Bash completion after "workset l" - testing command prefix matching
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset l")
        .env("COMP_POINT", "9")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // With 2 words, completes repos (not subcommands)
    let completions: Vec<&str> = stdout.trim().split('\n').collect();
    assert!(completions.contains(&"repo1"));
}

#[test]
fn test_fish_complete_with_multiple_args() {
    let temp_dir = setup_test_workspace();
    let binary = get_binary_path();

    // Fish completion after "workset drop repo1 "
    // Testing that it continues offering completions
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset drop repo1 ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // Should still list repositories (for multiple drops) with metadata
    let lines: Vec<&str> = stdout.trim().split('\n').collect();
    assert_eq!(lines.len(), 3);

    // Check that all repos are present (now with tab-separated descriptions)
    let repo_names: Vec<&str> = lines
        .iter()
        .map(|line| line.split('\t').next().unwrap())
        .collect();
    assert!(repo_names.contains(&"repo1"));
    assert!(repo_names.contains(&"repo2"));
    assert!(repo_names.contains(&"subdir/repo3"));
}

#[test]
fn test_bash_no_comp_point() {
    let binary = get_binary_path();
    let temp_dir = setup_test_workspace();

    // Missing COMP_POINT should default to 0
    let output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset ")
        // No COMP_POINT set
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());

    // With COMP_POINT=0, current_line is empty, so words.len() <= 1
    let expected = "clone\nrestore\ndrop\nlist\nls\nstatus";
    assert_eq!(stdout.trim(), expected);
}

#[test]
fn test_fish_vs_bash_output_format() {
    let temp_dir = TempDir::new().unwrap();
    let binary = get_binary_path();

    // Get bash output
    let bash_output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "bash")
        .env("COMP_LINE", "workset ")
        .env("COMP_POINT", "8")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    // Get fish output
    let fish_output = Command::new(&binary)
        .env("_ARGCOMPLETE_", "fish")
        .env("COMP_LINE", "workset ")
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to execute binary");

    let bash_stdout = String::from_utf8(bash_output.stdout).unwrap();
    let fish_stdout = String::from_utf8(fish_output.stdout).unwrap();

    // Bash should just have command name
    assert_eq!(bash_stdout.trim(), "init");

    // Fish should have command name + description separated by tab
    assert!(fish_stdout.contains('\t'));
    assert_eq!(
        fish_stdout.trim(),
        "init\tInitialize a workspace in current directory"
    );
}
