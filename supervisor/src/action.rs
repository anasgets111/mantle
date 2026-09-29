use shared::warn;

/// Decodes a command into its capability's action enum, logging and returning `None` when the
/// action is unknown or its arguments do not fit the variant. The Renderer already raised on a
/// misfit at the config's call; this decode is the trust boundary (ADR-0291, ADR-0114).
pub(crate) fn parse_action<A: serde::de::DeserializeOwned>(params: &shared::CommandParams) -> Option<A> {
    shared::action::decode(&params.action, &params.arguments)
        .map_err(|err| {
            warn!(
                "malformed {}.{} command from generation {}: {err}; arguments {:?}",
                params.capability, params.action, params.generation_id, params.arguments
            )
        })
        .ok()
}
