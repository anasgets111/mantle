use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod framing;
mod secure_buffer;
pub use secure_buffer::SecureBuffer;
pub use zeroize::Zeroize;

/// Where the control socket lives, derived from `$XDG_RUNTIME_DIR`. Both
/// `supervisor` (the listener) and `renderer` (the client) resolve this the same way, so it
/// lives here instead of being reimplemented on each side (build-steps.md Phase 9: not
/// `/tmp`, which is world-writable and unsuitable for a socket that will eventually carry
/// secure textfield submissions, ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-shell.sock"))
}

/// Guarded JSON-RPC 2.0 envelope wrapping a Lua write action.
/// See docs/oblisk-idl-api-specs.md §7.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub jsonrpc: String,
    pub method: String,
    pub params: CommandParams,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandParams {
    pub generation_id: u32,
    pub capability: String,
    pub action: String,
    pub arguments: Vec<serde_json::Value>,
    pub expected_revision: u32,
}

/// Emitted by the Supervisor on system changes to hydrate active Lua signals.
/// `revision` is the capability's state-version counter (see ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub revision: u32,
    pub payload: serde_json::Value,
}

/// Identifies a connection's generation. Sent as the very first frame on every new
/// control-socket connection, before any other traffic, so the Supervisor's listener can
/// address commands and pushes to the right generation instead of assuming exactly one peer
/// (build-steps.md Phase 9; CONTEXT.md's Candidate and Authoritative generation entries).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionHandshake {
    pub generation_id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_envelope_matches_idl_wire_format() {
        // Exact example from docs/oblisk-idl-api-specs.md §7.2.
        let wire = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "ExecuteCommand",
            "params": {
                "generation_id": 4,
                "capability": "audio",
                "action": "set_volume",
                "arguments": [0.75],
                "expected_revision": 42
            },
            "id": 105
        });

        let envelope: CommandEnvelope = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(envelope.jsonrpc, "2.0");
        assert_eq!(envelope.method, "ExecuteCommand");
        assert_eq!(envelope.id, 105);
        assert_eq!(envelope.params.generation_id, 4);
        assert_eq!(envelope.params.capability, "audio");
        assert_eq!(envelope.params.action, "set_volume");
        assert_eq!(envelope.params.expected_revision, 42);

        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
    }

    #[test]
    fn state_snapshot_round_trips() {
        let snapshot = StateSnapshot {
            revision: 42,
            payload: serde_json::json!({"volume": 0.75}),
        };

        let wire = serde_json::to_value(&snapshot).unwrap();
        let parsed: StateSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.revision, snapshot.revision);
        assert_eq!(parsed.payload, snapshot.payload);
    }

    #[test]
    fn connection_handshake_round_trips() {
        let handshake = ConnectionHandshake { generation_id: 3 };
        let wire = serde_json::to_value(handshake).unwrap();
        assert_eq!(wire, serde_json::json!({ "generation_id": 3 }));

        let parsed: ConnectionHandshake = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, handshake);
    }
}
