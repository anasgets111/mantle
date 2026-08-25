# In-place reload explicitly resets Supervisor-held registrations

In-place reload (ADR-0001) resets the Renderer's Lua VM and re-runs `shell.lua`, which re-issues every `idle:register_threshold(...)` call. That registration state lives in the Supervisor, a separate process, so nothing about the Renderer's VM reset is visible there. A second `register` call reads as a new threshold, not a replacement, leaking duplicate `ext_idle_notification_v1` listeners on every value-change reload.

Both prior-art projects solve the same-process version of this for free: Quickshell's `IdleMonitor` extends `PostReloadHook`, so every reload constructs a fresh object and the old one's destructor tears down its listener; Noctalia's `IdleManager::reload(config)` calls `clearBehaviors()` unconditionally before rebuilding every behavior from the new config, no diffing. Neither trick crosses a process boundary, because in both projects the registration and the reload live in the same process.

Decision: in-place reload sends one explicit IPC message before re-running the script: capability `"renderer"`, action `"reset_registrations"`. The Supervisor drops every registration tied to that `generation_id` before the fresh top-level run repopulates them. This is the cross-process equivalent of `clearBehaviors()`: unconditional clear, then rebuild, no dedup-by-key needed on either side.
