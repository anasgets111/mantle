use shared::warn;

/// Decodes a command into its capability's action enum, logging and returning `None` when the
/// action is unknown or its arguments do not fit the variant. The Renderer already raised on a
/// misfit at the config's call; this decode is the trust boundary (ADR-0291, ADR-0114).
pub(crate) fn parse_action<A: serde::de::DeserializeOwned>(params: &shared::CommandParams) -> Option<A> {
    shared::action::decode(&params.action, &params.arguments).map_err(|err| warn!("{}", malformed(params, &err))).ok()
}

/// The log line for a command that failed to decode: neither serde's message nor the arguments,
/// since either can hold a password from a network or secrets action (#121). Only the category,
/// as `decode` reads values, not text, so serde has no position to give.
fn malformed(params: &shared::CommandParams, err: &serde_json::Error) -> String {
    let shared::CommandParams { capability, action, generation_id, .. } = params;
    format!("malformed {capability}.{action} command from generation {generation_id}: {:?} error", err.classify())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::envelope;

    #[test]
    fn a_malformed_command_is_logged_without_its_arguments_or_serde_message() {
        let params = envelope(shared::Capability::Lock, "set_unlock_animation", vec!["hunter2".into()]).params;
        let err = shared::action::decode::<shared::action::LockAction>(&params.action, &params.arguments).unwrap_err();
        assert_eq!(
            malformed(&params, &err),
            "malformed lock.set_unlock_animation command from generation 7: Data error"
        );
    }
}
