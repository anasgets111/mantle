Use [`persistent_table`](../guide/scripting.md#persistent_table), which sends these actions for you.

<!-- reference -->

## Backend

A write pushes at once. Another writer's change, another shell's included, refills missing defaults
and pushes. A relative path is refused. Saving, outside edits and a broken file:
[`persistent_table`](../guide/scripting.md#persistent_table).
