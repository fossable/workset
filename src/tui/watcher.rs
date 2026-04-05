use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// A filesystem watcher with debouncing and path filtering.
///
/// This watcher:
/// - Uses notify's recommended watcher with a channel (per notify docs)
/// - Performs debouncing on the receive side to batch rapid changes
/// - Filters out `.git` and `.workset` directory events
/// - Drains pending events after refresh to prevent feedback loops
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<Result<Event, notify::Error>>,
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

    /// Check if a refresh is needed due to filesystem changes.
    ///
    /// This drains all pending events, filters them, and applies debouncing.
    /// Returns `true` if enough relevant events occurred and sufficient time
    /// has passed since the last refresh.
    pub fn poll_refresh(&mut self) -> bool {
        let dominated = self.drain_relevant_events();

        if dominated && self.last_refresh.elapsed() > self.debounce_duration {
            self.last_refresh = Instant::now();
            return true;
        }

        false
    }

    /// Drain all pending events and discard them.
    ///
    /// Call this after performing a refresh to prevent feedback loops where
    /// the refresh operation itself generates filesystem events.
    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.last_refresh = Instant::now();
    }

    /// Drain all pending events and return whether any relevant events occurred.
    fn drain_relevant_events(&mut self) -> bool {
        let mut has_relevant = false;

        loop {
            match self.rx.try_recv() {
                Ok(Ok(event)) => {
                    if Self::is_relevant_event(&event) {
                        has_relevant = true;
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

        has_relevant
    }

    /// Check if an event is relevant (not in .git or .workset directories).
    fn is_relevant_event(event: &Event) -> bool {
        event.paths.iter().any(|path| {
            let path_str = path.to_string_lossy();
            !path_str.contains("/.git/")
                && !path_str.contains("/.workset/")
                && !path_str.ends_with("/.git")
                && !path_str.ends_with("/.workset")
        })
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
        let first_refresh = watcher.poll_refresh();

        // Immediately create another file
        fs::write(temp_dir.path().join("test2.txt"), "world").unwrap();
        std::thread::sleep(Duration::from_millis(10));

        // Second poll should NOT trigger refresh (within debounce window)
        let second_refresh = watcher.poll_refresh();

        // Wait for debounce period to pass
        std::thread::sleep(Duration::from_millis(150));

        // Create another file
        fs::write(temp_dir.path().join("test3.txt"), "!").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Third poll should trigger refresh (debounce period passed)
        let third_refresh = watcher.poll_refresh();

        assert!(first_refresh, "First refresh should trigger");
        assert!(!second_refresh, "Second refresh should be debounced");
        assert!(third_refresh, "Third refresh should trigger after debounce period");
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
        let refresh = watcher.poll_refresh();
        assert!(!refresh, ".git directory changes should be filtered");

        // But regular file changes should trigger
        fs::write(temp_dir.path().join("regular.txt"), "test").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let refresh = watcher.poll_refresh();
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
        let refresh = watcher.poll_refresh();
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
        assert!(watcher.poll_refresh());

        // Simulate refresh operation that creates events
        fs::write(temp_dir.path().join("test2.txt"), "world").unwrap();
        std::thread::sleep(Duration::from_millis(10));

        // Drain pending events (as we do after refresh)
        watcher.drain_pending();

        // Wait past debounce period
        std::thread::sleep(Duration::from_millis(100));

        // Poll should NOT trigger because events were drained
        let refresh = watcher.poll_refresh();
        assert!(!refresh, "Drained events should not trigger refresh");
    }
}
