Declare files with [`persistent_table`](../guide/scripting.md#persistent_table): it sends these
actions and exposes each key as a signal. Saving, outside edits and a broken file are covered
there.

<!-- reference -->

## Backend

Plain JSON files at absolute paths, watched with inotify. A `set` pushes at once; the save follows
1 s later. Another writer's change, another shell's included, gets missing defaults refilled and
pushes.
