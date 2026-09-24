```lua
rect {
    width = 8,
    height = 8,
    radius = 4,
    background = "#F38BA8",
    visible = mantle.privacy:map(function(privacy)
        return privacy ~= nil and (#privacy.microphone_users > 0 or #privacy.camera_users > 0)
    end),
}
```

<!-- reference -->

## Backend

| Source | Feeds |
| :--- | :--- |
| `Stream/Input/Audio`, minus monitor captures | `privacy` microphone users |
| `Stream/Output/Video` | `privacy` screencast users; wlr-screencopy clients are invisible |
| `/proc/*/fd` holders of `/dev/videoN`, rescanned on inotify open/close | `privacy` camera users; `Video/Source` nodes only enrich names. Devices are enumerated once |

The PipeWire sources share [`audio`'s thread](audio.md#backend), and its failure mode.

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a microphone or camera indicator | Map `microphone_users` and `camera_users`, as in the example above |
