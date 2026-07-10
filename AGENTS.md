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

## TODO list

- Shell autocomplete not working
  - It should suggest repos in the workspace to drop
  - It should suggest repos in the library to restore
- Support mirroring to multiple remotes
- Instead of the word "Workspace", show the workspace path as the title of the
  workspace panel
  - If too long, truncate with ellipses
- Remove status bar
  - Instead show temporary status on a per repo-basis where the "last modified"
    timestamp currently is (end of the line)
- Only show search bar when typing
- Add an "info" panel above "Library"?
  - If a repo is selected, show stats
    - Total size
    - Clean or number of outstanding changes
    - Show mirror status
  - If a remote is selected, show available repos
- Repo status doesn't change to "clean" if commit is pushed while TUI is open
