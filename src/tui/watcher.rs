use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// Filesystem activity observed by a single `FileWatcher::poll` call
#[derive(Default)]
pub struct WatchSignals {
    /// Worktree files changed (debounced): the repo data should be reloaded
    pub refresh: bool,
    /// Repos whose remote-tracking refs changed, e.g. because a `git push`
    /// ran in another terminal; candidates for a remote sync check
    pub refs_changed: Vec<PathBuf>,
}

/// How one watched path affects the signals
enum PathClass {
    Worktree,
    /// A `.git/refs/remotes` change; carries the repo root
    RemoteRefs(PathBuf),
    Ignored,
}

/// A filesystem watcher with debouncing and path filtering.
///
/// This watcher:
/// - Uses notify's recommended watcher with a channel (per notify docs)
/// - Performs debouncing on the receive side to batch rapid changes
/// - Filters out `.workset` and most `.git` internals, except
///   `.git/refs/remotes` changes which are reported per repo
/// - Drains pending events after refresh to prevent feedback loops
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<Result<notify::Event, notify::Error>>,
    last_refresh: Instant,
    debounce_duration: Duration,
}

impl FileWatcher {
    /// Create a new file watcher for the given path.
    pub fn new(path: &Path, debounce_duration: Duration) -> Result<Self, notify::Error> {
        let (tx, rx) = channel();
        let mut watcher = recommended_watcher(tx)?;
        watcher.watch(path, RecursiveMode::Recursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
            last_refresh: Instant::now(),
            debounce_duration,
        })
    }

    /// Drain all pending events, classify them, and apply debouncing to the
    /// refresh signal. Remote-ref changes are reported per repo, undebounced
    /// (the sync scheduler applies its own cooldown).
    pub fn poll(&mut self) -> WatchSignals {
        let mut worktree_changed = false;
        let mut refs_changed: Vec<PathBuf> = Vec::new();

        loop {
            match self.rx.try_recv() {
                Ok(Ok(event)) => {
                    // Reads (e.g. directory traversal) don't change anything
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        continue;
                    }
                    for path in &event.paths {
                        match Self::classify(path) {
                            PathClass::Worktree => worktree_changed = true,
                            PathClass::RemoteRefs(repo_root) => {
                                if !refs_changed.contains(&repo_root) {
                                    refs_changed.push(repo_root);
                                }
                            }
                            PathClass::Ignored => {}
                        }
                    }
                }
                Ok(Err(_)) => {
                    // Watch error - ignore
                }
                Err(TryRecvError::Disconnected) | Err(TryRecvError::Empty) => {
                    break;
                }
            }
        }

        let refresh = worktree_changed && self.last_refresh.elapsed() > self.debounce_duration;
        if refresh {
            self.last_refresh = Instant::now();
        }
        WatchSignals {
            refresh,
            refs_changed,
        }
    }

    /// Drain all pending events and discard them.
    ///
    /// Call this after performing a refresh to prevent feedback loops where
    /// the refresh operation itself generates filesystem events.
    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.last_refresh = Instant::now();
    }

    /// Classify a watched path. `.workset` and `.git` internals are noise,
    /// except `.git/refs/remotes/*` which signals that a push or fetch
    /// touched the repo's remote-tracking refs. `.git/refs/heads` is
    /// deliberately ignored: local commits shouldn't trigger network checks.
    fn classify(path: &Path) -> PathClass {
        let mut repo_root = PathBuf::new();
        let mut components = path.components();
        while let Some(component) = components.next() {
            if let Component::Normal(name) = component {
                if name == ".workset" {
                    return PathClass::Ignored;
                }
                if name == ".git" {
                    let mut rest = components.map(|c| c.as_os_str());
                    if rest.next().is_some_and(|c| c == "refs")
                        && rest.next().is_some_and(|c| c == "remotes")
                    {
                        return PathClass::RemoteRefs(repo_root);
                    }
                    return PathClass::Ignored;
                }
            }
            repo_root.push(component);
        }
        PathClass::Worktree
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_debouncing_prevents_rapid_refreshes() {
        let temp_dir = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(temp_dir.path(), Duration::from_millis(100)).unwrap();

        // Wait for debounce period to pass (watcher starts with last_refresh = now)
        std::thread::sleep(Duration::from_millis(150));

        // Create a file to trigger an event
        fs::write(temp_dir.path().join("test.txt"), "hello").unwrap();

        // Wait for the event to be detected
        std::thread::sleep(Duration::from_millis(50));

        // First poll should trigger refresh (debounce period has passed)
        let first_refresh = watcher.poll().refresh;

        // Immediately create another file
        fs::write(temp_dir.path().join("test2.txt"), "world").unwrap();
        std::thread::sleep(Duration::from_millis(10));

        // Second poll should NOT trigger refresh (within debounce window)
        let second_refresh = watcher.poll().refresh;

        // Wait for debounce period to pass
        std::thread::sleep(Duration::from_millis(150));

        // Create another file
        fs::write(temp_dir.path().join("test3.txt"), "!").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Third poll should trigger refresh (debounce period passed)
        let third_refresh = watcher.poll().refresh;

        assert!(first_refresh, "First refresh should trigger");
        assert!(!second_refresh, "Second refresh should be debounced");
        assert!(
            third_refresh,
            "Third refresh should trigger after debounce period"
        );
    }

    #[test]
    fn test_git_directory_filtered() {
        let temp_dir = TempDir::new().unwrap();
        let git_dir = temp_dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();

        let mut watcher = FileWatcher::new(temp_dir.path(), Duration::from_millis(50)).unwrap();

        // Create a file in .git directory
        fs::write(git_dir.join("config"), "test").unwrap();

        // Wait for events
        std::thread::sleep(Duration::from_millis(100));

        // Should not trigger refresh for .git changes
        let refresh = watcher.poll().refresh;
        assert!(!refresh, ".git directory changes should be filtered");

        // But regular file changes should trigger
        fs::write(temp_dir.path().join("regular.txt"), "test").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let refresh = watcher.poll().refresh;
        assert!(refresh, "Regular file changes should trigger refresh");
    }

    #[test]
    fn test_workset_directory_filtered() {
        let temp_dir = TempDir::new().unwrap();
        let workset_dir = temp_dir.path().join(".workset");
        fs::create_dir(&workset_dir).unwrap();

        let mut watcher = FileWatcher::new(temp_dir.path(), Duration::from_millis(50)).unwrap();

        // Create a file in .workset directory
        fs::write(workset_dir.join("data"), "test").unwrap();

        // Wait for events
        std::thread::sleep(Duration::from_millis(100));

        // Should not trigger refresh for .workset changes
        let refresh = watcher.poll().refresh;
        assert!(!refresh, ".workset directory changes should be filtered");
    }

    #[test]
    fn test_drain_pending_prevents_feedback_loop() {
        let temp_dir = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(temp_dir.path(), Duration::from_millis(50)).unwrap();

        // Create a file
        fs::write(temp_dir.path().join("test.txt"), "hello").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // First poll triggers refresh
        assert!(watcher.poll().refresh);

        // Simulate refresh operation that creates events
        fs::write(temp_dir.path().join("test2.txt"), "world").unwrap();
        std::thread::sleep(Duration::from_millis(10));

        // Drain pending events (as we do after refresh)
        watcher.drain_pending();

        // Wait past debounce period
        std::thread::sleep(Duration::from_millis(100));

        // Poll should NOT trigger because events were drained
        let refresh = watcher.poll().refresh;
        assert!(!refresh, "Drained events should not trigger refresh");
    }

    #[test]
    fn test_classify_paths() {
        let class = FileWatcher::classify(Path::new("/ws/repo/.git/refs/remotes/origin/main"));
        assert!(matches!(class, PathClass::RemoteRefs(root) if root == Path::new("/ws/repo")));
        assert!(matches!(
            FileWatcher::classify(Path::new("/ws/repo/.git/refs/heads/main")),
            PathClass::Ignored
        ));
        assert!(matches!(
            FileWatcher::classify(Path::new("/ws/repo/.git/index")),
            PathClass::Ignored
        ));
        assert!(matches!(
            FileWatcher::classify(Path::new("/ws/.workset/other/config")),
            PathClass::Ignored
        ));
        assert!(matches!(
            FileWatcher::classify(Path::new("/ws/repo/src/main.rs")),
            PathClass::Worktree
        ));
    }

    #[test]
    fn test_remote_ref_change_reports_repo() {
        let temp_dir = TempDir::new().unwrap();
        let refs_dir = temp_dir.path().join("repo/.git/refs/remotes/origin");
        fs::create_dir_all(&refs_dir).unwrap();

        let mut watcher = FileWatcher::new(temp_dir.path(), Duration::from_millis(50)).unwrap();

        fs::write(refs_dir.join("main"), "0000").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let signals = watcher.poll();
        assert!(!signals.refresh, "ref changes should not trigger a refresh");
        assert_eq!(signals.refs_changed.len(), 1);
        assert!(
            signals.refs_changed[0].ends_with("repo"),
            "signal should carry the repo root, got {:?}",
            signals.refs_changed[0]
        );
    }
}
