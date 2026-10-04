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
| Running `Stream/Input/Audio` PipeWire nodes | `microphone_users` |
| Running `Stream/Output/Video` and portal `Video/Source` PipeWire nodes | `screencast_users` |
| `/proc/*/fd` links to a `/dev/videoN`, rescanned on each inotify open or close of the device | `camera_users`. A PipeWire `Video/Source` from the same pid only supplies the name |

The PipeWire half shares [`audio`'s thread](audio.md#backend). `privacy` pushes its first `/proc`
scan as soon as it starts. While PipeWire is down, or before it first answers, the microphone and
screencast lists are empty and cameras lose their PipeWire names; all refill when it reconnects.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| A webcam plugged in after `privacy` started never shows | The device list is read once at start. Restart the Supervisor |
| `grim` or `wf-recorder` never shows in `screencast_users` | wlr-screencopy bypasses PipeWire; only portal screen captures appear |
