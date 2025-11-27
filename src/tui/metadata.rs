use anyhow::Result;
use std::path::Path;
use std::time::SystemTime;


/// Format a SystemTime as a human-readable "time ago" string
pub fn format_time_ago(time: SystemTime) -> String {
    let elapsed = match SystemTime::now().duration_since(time) {
        Ok(d) => d,
        Err(_) => {
            // Time is in the future, should not happen
            return "just now".to_string();
        }
    };

    let seconds = elapsed.as_secs();

    if seconds < 60 {
        format!("{}s ago", seconds)
    } else if seconds < 3600 {
        // Under 1 hour: show minutes (rounded)
        let minutes = (seconds + 30) / 60; // Round to nearest minute
        format!("{}m ago", minutes)
    } else if seconds < 86400 {
        // Under 1 day: show hours (rounded)
        let hours = (seconds + 1800) / 3600; // Round to nearest hour
        format!("{}h ago", hours)
    } else if seconds < 2592000 {
        // Under 30 days: show days (rounded)
        let days = (seconds + 43200) / 86400; // Round to nearest day
        format!("{}d ago", days)
    } else if seconds < 31536000 {
        // Under 1 year: show months (rounded)
        let months = (seconds + 1296000) / 2592000; // Round to nearest month
        format!("{}mo ago", months)
    } else {
        // Over 1 year: show years (rounded)
        let years = (seconds + 15768000) / 31536000; // Round to nearest year
        format!("{}y ago", years)
    }
}

/// Format bytes as human-readable size
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Get the last modification time for a repository
/// For clean repos, use last commit time. For dirty repos, use max of commit time or dirty files.
pub fn get_repo_modification_time(repo_path: &Path, is_clean: bool) -> Result<SystemTime> {
    if is_clean {
        // For clean repos, get the last commit time
        let commit_time = get_last_commit_time(repo_path)?;
        // debug!(
        //     repo = ?repo_path,
        //     is_clean = true,
        //     timestamp = %format_timestamp_debug(commit_time),
        //     time_ago = %format_time_ago(commit_time),
        //     "Repository modification time (clean repo, using commit time)"
        // );
        Ok(commit_time)
    } else {
        // For dirty repos, get the max of last commit time and dirty file modification times
        let commit_time = get_last_commit_time(repo_path).unwrap_or(SystemTime::UNIX_EPOCH);
        let dirty_files_time =
            get_dirty_files_modification_time(repo_path).unwrap_or(SystemTime::UNIX_EPOCH);
        let max_time = commit_time.max(dirty_files_time);

        // debug!(
        //     repo = ?repo_path,
        //     is_clean = false,
        //     commit_timestamp = %format_timestamp_debug(commit_time),
        //     commit_time_ago = %format_time_ago(commit_time),
        //     dirty_files_timestamp = %format_timestamp_debug(dirty_files_time),
        //     dirty_files_time_ago = %format_time_ago(dirty_files_time),
        //     max_timestamp = %format_timestamp_debug(max_time),
        //     max_time_ago = %format_time_ago(max_time),
        //     "Repository modification time (dirty repo, using max)"
        // );

        Ok(max_time)
    }
}

/// Get the last commit time using git
fn get_last_commit_time(repo_path: &Path) -> Result<SystemTime> {
    use std::process::Command;

    let output = Command::new("git")
        .args([
            "-C",
            repo_path.to_str().unwrap(),
            "log",
            "-1",
            "--format=%ct",
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to get last commit time");
    }

    let timestamp_str = String::from_utf8(output.stdout)?;
    let timestamp: i64 = timestamp_str.trim().parse()?;
    let system_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64);

    // debug!(
    //     repo = ?repo_path,
    //     unix_timestamp = timestamp,
    //     "Last commit time from git"
    // );

    Ok(system_time)
}

/// Get the most recent modification time of dirty files
fn get_dirty_files_modification_time(repo_path: &Path) -> Result<SystemTime> {
    use std::process::Command;

    // Get list of modified/untracked files
    let output = Command::new("git")
        .args(["-C", repo_path.to_str().unwrap(), "status", "--porcelain"])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to get dirty files");
    }

    let mut latest_time = SystemTime::UNIX_EPOCH;

    for line in String::from_utf8(output.stdout)?.lines() {
        if line.len() < 4 {
            continue;
        }

        // Extract filename from git status output
        let filename = line[3..].trim();
        let file_path = repo_path.join(filename);

        if let Ok(metadata) = std::fs::metadata(&file_path)
            && let Ok(modified) = metadata.modified()
        {
            if modified > latest_time {
                latest_time = modified;
            }
        }
    }

    // debug!(
    //     repo = ?repo_path,
    //     dirty_files_checked = file_count,
    //     latest_timestamp = %format_timestamp_debug(latest_time),
    //     "Dirty files modification time scan complete"
    // );

    // Return the latest time found (could be UNIX_EPOCH if no files found)
    Ok(latest_time)
}

/// Calculate total size of a repository on disk
pub fn get_repo_size(repo_path: &Path) -> Result<u64> {
    use std::fs;

    let mut total_size = 0u64;

    fn visit_dirs(dir: &Path, total: &mut u64) -> Result<()> {
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();

                // Skip .git directory for more accurate size
                if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    continue;
                }

                if path.is_dir() {
                    visit_dirs(&path, total)?;
                } else if let Ok(metadata) = fs::metadata(&path) {
                    *total += metadata.len();
                }
            }
        }
        Ok(())
    }

    visit_dirs(repo_path, &mut total_size)?;
    Ok(total_size)
}
