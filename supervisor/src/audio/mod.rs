//! PipeWire-backed audio state (build-steps.md Phase 6). Only `mixer` (the per-app stream
//! tracker) exists so far; master volume/mute, default sink/source routing, and BlueZ codec
//! control (`docs/oblisk-supervisor-services-dbus.md` §6) are later phases.

pub mod mixer;
