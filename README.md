<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/workset/master/.github/images/workset-256.png" />
</p>

![License](https://img.shields.io/github/license/fossable/workset)
![Build](https://github.com/fossable/workset/actions/workflows/test.yml/badge.svg)
![GitHub repo size](https://img.shields.io/github/repo-size/fossable/workset)
![Stars](https://img.shields.io/github/stars/fossable/workset?style=social)

<hr>

**workset** is yet another tool for managing git repos locally.

### Worksets

Simply defined, a _working set_ is all of the repositories you need to work on
at any given time. Even more simply, your _workspace_ is the local directory
where you keep your Git repositories.

For many developers, our Git workspaces tend to become "libraries" that contains
all the projects we've ever worked on, spanning multiple Git providers. A quick
survey revealed that I had 46 personal repos in my workspace and 192 for my job.
With all of that junk piled up, it becomes hard to remember what repos have
outstanding changes that still need to be completed.

The principle behind **workset** is that your workspace should only consist of
your _working set_. This reduces unnecessary noise as repositories accumulate in
your workspace over time and improves indexing performance of your development
tools.

Adhering to this principle involves frequently cloning and deleting repositories
from your workspace, so `workset` makes these mechanics _fast_ and _easy_. When
repositories are dropped from your workspace, they are just saved locally in a
_library_ so restoring them later can be done in an instant.

### Github can kill you

Maybe not actually, but they can delete your account along with your repos any
day of the week. It has happened before. Don't let them have full control of
your repos! **workset** keeps full mirrors of all of your repos locally to keep
them safe.

### Quickstart

All `workset` commands run in reference to the current directory.

```sh
# Initialize a new workspace in the current directory
❯ workset init

# Add a repository to your working set
❯ workset github.com/jqlang/jq

# The repository's local path always reflects the remote path
❯ cd ./github.com/jqlang/jq

# Drop the repo from the working set (it remains in the library)
❯ cd ..
❯ workset drop ./jq

# Or, you can drop all repositories in the current directory (any that have
# unpushed changes will not be touched).
❯ workset drop

# If you don't want a repo to remain in the library, use --delete
❯ workset drop --delete ./delete_this_repo

# When you need to work on a repository again, it's restored from the local library.
# Any upstream changes are also fetched.
❯ workset jq
```

#### Autocomplete

The shell autocomplete is smart enough to look at your CWD and suggest repos
that you might want to restore into your working set. Repos that were dropped
most recently are sorted first.

```sh
# Library contains: jqlang/jq

❯ workset jql[TAB]
jqlang/jq
```

#### TUI

Run `workset` without arguments to launch an interactive TUI from the current
directory:

```sh
# Launch interactive repository browser
❯ workset

# Use j/k or arrow keys to navigate
# Press / to fuzzy find repositories
# Press Enter to add the selected repository to your working set
# Press q to quit
```
