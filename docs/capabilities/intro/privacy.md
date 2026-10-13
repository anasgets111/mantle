```lua
rect {
    width = 8,
    height = 8,
    radius = 4,
    background = "#F38BA8",
    visible = mantle.privacy:map(function(privacy)
        return #privacy.microphone_users > 0 or #privacy.camera_users > 0
    end, false),
}
```

<!-- reference -->

## Backend

| Source | Feeds |
| :--- | :--- |
| Running `Stream/Input/Audio` PipeWire nodes | `microphone_users` |
| Running `Stream/Output/Video` and portal `Video/Source` PipeWire nodes | `screencast_users` |
| Running `Stream/Input/Video` PipeWire nodes linked to a camera `Video/Source` | `camera_users`, named by the app. A viewer of a screen-share source is not counted |
| `/proc/*/fd` links to a `/dev/videoN`, rescanned on each inotify open or close of the device | `camera_users` for apps using V4L2 directly. The PipeWire daemon is skipped, since it holds the device for the apps above |

The PipeWire half shares [`audio`'s thread](audio.md#backend). `privacy` pushes its first `/proc`
scan as soon as it starts. While PipeWire is down, or before it first answers, the microphone and
screencast lists are empty and cameras used through PipeWire are missing; all refill when it
reconnects.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `grim` or `wf-recorder` never shows in `screencast_users` | wlr-screencopy bypasses PipeWire; only portal screen captures appear |
