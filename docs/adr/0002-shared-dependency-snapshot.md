---
status: accepted
---

# Use one dependency snapshot for loading and watching

The rooted loader and the supervisor watcher use one dependency snapshot module.
Each capture opens every file once, records inode and hash from the opened
descriptor, and returns bounded watch roots. Before commit, the module
re-captures expected files and compares their identities and hashes.

On cold start, the watcher monitors the configured entry file, trusted include
roots, and parent directories needed for missing modules. After a successful
generation, it watches only that generation's dependency snapshot.

## Consequences

- Path races stay in one module.
- A failed candidate cannot replace the successful watch set.
- Cold start watches more directories until the first valid snapshot exists.
