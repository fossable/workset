This tool manages git repositories in two directories: a "workspace" for active
repos and a "library" of inactive repos. Moving single or groups of repos
between the two should be quick and easy.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | Local directory (default: `~/.workset`) where **workset** keeps your repos when they're not in your workspace.                                                                      |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. Only "clean" repos without uncommitted changes can be dropped. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

## Mirroring

Repos with multiple remotes are kept in sync automatically: commits the user
has pushed to at least one remote are mirrored to the others (all shared
branches plus tags). Commits that exist only locally are never pushed. Sync
runs in the background on TUI startup, after the interactive shell exits, when
a push from another terminal updates `.git/refs/remotes`, and periodically
while the TUI is open. Diverged refs are reported as errors, never
force-pushed. The core logic lives in `src/sync.rs`; the TUI scheduling in
`SyncManager` (`src/tui/mod.rs`). `workset sync` runs the same logic from the
CLI.

## Testing

Unit tests live beside the code (`cargo test`). End-to-end tests are
[attest](https://github.com/fossable/attest) shell tests in `tests/`, which
drive the `workset` binary from `PATH`:

```sh
cargo build
attest --bin-dir target/debug tests/
```

## TODO list

- Add an "info" panel above "Library"?
  - If a repo is selected, show stats
    - Total size
    - Clean or number of outstanding changes
    - Show mirror status
  - If a remote is selected, show available repos
- Per-remote mirroring opt-out (e.g. git config `workset.noMirror`) for
  remotes the user can't push to, like a fork's upstream
