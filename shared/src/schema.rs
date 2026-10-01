//! Schemas used by stub generation and the Renderer build.

use schemars::{Schema, schema_for};

mod samples;
pub use samples::check_samples;

/// Capability payload and action schemas used by stubs and build-time check samples.
/// `None` means no actions.
pub fn capability_schemas() -> Vec<(&'static str, Schema, Option<Schema>)> {
    vec![
        (
            "applications",
            schema_for!(crate::state::applications::ApplicationsState),
            Some(schema_for!(crate::action::ApplicationsAction)),
        ),
        ("audio", schema_for!(crate::state::audio::AudioState), Some(schema_for!(crate::action::AudioAction))),
        ("battery", schema_for!(crate::state::battery::BatteryState), None),
        ("idle", schema_for!(crate::state::idle::IdleState), None),
        (
            "bluetooth",
            schema_for!(crate::state::bluetooth::BluetoothState),
            Some(schema_for!(crate::action::BluetoothAction)),
        ),
        (
            "brightness",
            schema_for!(crate::state::brightness::BrightnessState),
            Some(schema_for!(crate::action::BrightnessAction)),
        ),
        ("files", schema_for!(crate::state::files::FilesState), Some(schema_for!(crate::action::FilesAction))),
        (
            "processes",
            schema_for!(crate::state::processes::ProcessesState),
            Some(schema_for!(crate::action::ProcessesAction)),
        ),
        (
            "keyboard",
            schema_for!(crate::state::keyboard::KeyboardState),
            Some(schema_for!(crate::action::KeyboardAction)),
        ),
        ("lock", schema_for!(crate::state::lock::LockState), Some(schema_for!(crate::action::LockAction))),
        ("mpris", schema_for!(crate::state::mpris::MprisState), Some(schema_for!(crate::action::MprisAction))),
        ("network", schema_for!(crate::state::network::NetworkState), Some(schema_for!(crate::action::NetworkAction))),
        ("secrets", schema_for!(crate::state::secrets::SecretsState), None),
        (
            "notifications",
            schema_for!(crate::state::notifications::NotificationsState),
            Some(schema_for!(crate::action::NotificationsAction)),
        ),
        ("power", schema_for!(crate::state::power::PowerState), Some(schema_for!(crate::action::PowerAction))),
        ("privacy", schema_for!(crate::state::privacy::PrivacyState), None),
        ("sysinfo", schema_for!(crate::state::sysinfo::SysinfoState), Some(schema_for!(crate::action::SysinfoAction))),
        ("system", schema_for!(crate::state::system::SystemState), Some(schema_for!(crate::action::SystemAction))),
        ("storage", schema_for!(crate::state::storage::StorageState), Some(schema_for!(crate::action::StorageAction))),
        ("polkit", schema_for!(crate::state::polkit::PolkitState), Some(schema_for!(crate::action::PolkitAction))),
        ("tray", schema_for!(crate::state::tray::TrayState), Some(schema_for!(crate::action::TrayAction))),
        ("updates", schema_for!(crate::state::updates::UpdatesState), Some(schema_for!(crate::action::UpdatesAction))),
        (
            "workspaces",
            schema_for!(crate::state::workspaces::WorkspacesState),
            Some(schema_for!(crate::action::WorkspacesAction)),
        ),
        ("windows", schema_for!(crate::state::windows::WindowsState), Some(schema_for!(crate::action::WindowsAction))),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    #[test]
    fn every_capability_has_a_schema() {
        let declared: BTreeSet<&str> = super::capability_schemas().into_iter().map(|(name, ..)| name).collect();
        let expected: BTreeSet<&str> = crate::Capability::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(declared, expected, "capability_schemas is out of step with Capability::ALL");
    }
}
