//! `mantle.privacy` detects camera use (ADR-0034). Kernel `/dev/videoN` opener detection
//! (`video.rs`) covers raw V4L2 users; PipeWire camera consumers (`Stream/Input/Video` linked to a
//! camera source) come from `audio::mixer`'s existing registry thread. No second PipeWire
//! connection, hardware thread, or D-Bus proxy.

pub mod controller;
pub mod video;

pub use controller::PrivacyController;
