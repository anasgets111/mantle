//! `mantle.privacy` snapshot payload.

use serde::Serialize;

/// One app using a camera, microphone or screen capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PrivacyUser {
    /// PipeWire `application.name`, else `/proc/<pid>/comm`, else `"pid 1234"` (or `"node 56"`);
    /// never empty.
    pub app_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PrivacyState {
    /// One entry per process holding a `/dev/videoN` open; empty when none is. Only devices present
    /// when `privacy` started are watched.
    pub camera_users: Vec<PrivacyUser>,
    /// Apps with a running PipeWire audio capture, one per name (ADR-0137). Idle streams and
    /// sink-monitor captures are absent; a muted microphone still counts.
    pub microphone_users: Vec<PrivacyUser>,
    /// Apps with a running PipeWire screen-capture stream, one per name (ADR-0137).
    /// wlr-screencopy tools such as `wf-recorder` and `grim` never appear.
    pub screencast_users: Vec<PrivacyUser>,
}
