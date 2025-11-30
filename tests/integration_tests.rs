use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Get the path to the compiled binary
fn get_binary_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let debug_path = PathBuf::from(manifest_dir).join("target/debug/workset");
    let release_path = PathBuf::from(manifest_dir).join("target/release/workset");

    if release_path.exists() {
        release_path
    } else if debug_path.exists() {
        debug_path
    } else {
        panic!("Binary not found. Run 'cargo build' first.");
    }
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
        .args(["test-repo"])
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
        .args(["github.com/testuser/nested-repo"])
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
            .args(["cycle-repo"])
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
        .args(["main-repo"])
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
