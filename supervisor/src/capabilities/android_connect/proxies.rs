//! Hand-written zbus proxy for `io.github.hepp3n.kdeconnect.Daemon`.

use zbus::proxy;

pub const SERVICE_NAME: &str = "io.github.hepp3n.kdeconnect";

/// Simplified device info over D-Bus.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, zbus::zvariant::Type)]
pub struct DbusDevice {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub is_paired: bool,
    pub is_reachable: bool,
}

#[proxy(
    interface = "io.github.hepp3n.kdeconnect.Daemon",
    default_service = "io.github.hepp3n.kdeconnect",
    default_path = "/io/github/hepp3n/kdeconnect/Daemon"
)]
pub trait Daemon {
    async fn list_devices(&self) -> zbus::Result<Vec<DbusDevice>>;
    async fn pair_device(&self, device_id: &str) -> zbus::Result<()>;
    async fn unpair_device(&self, device_id: &str) -> zbus::Result<()>;
    async fn send_ping(&self, device_id: &str, message: &str) -> zbus::Result<()>;
    async fn send_files(&self, device_id: &str, files: &[String]) -> zbus::Result<()>;
    async fn send_clipboard(&self, device_id: &str, content: &str) -> zbus::Result<()>;
    async fn share_clipboard(&self, device_id: &str) -> zbus::Result<()>;
    async fn ring_device(&self, device_id: &str) -> zbus::Result<()>;
    async fn browse_device(&self, device_id: &str) -> zbus::Result<()>;
    async fn unmount_device(&self, device_id: &str) -> zbus::Result<()>;
    async fn mounted_devices(&self) -> zbus::Result<Vec<String>>;
    async fn broadcast_identity(&self) -> zbus::Result<()>;
    async fn accept_pairing(&self, device_id: &str) -> zbus::Result<()>;
    async fn reject_pairing(&self, device_id: &str) -> zbus::Result<()>;
    async fn set_plugin_enabled(&self, device_id: &str, plugin_id: &str, enabled: bool) -> zbus::Result<()>;
    async fn get_disabled_plugins(&self, device_id: &str) -> zbus::Result<Vec<String>>;
    async fn push_local_commands(&self, device_id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn device_connected(&self, device_id: String, device: DbusDevice) -> zbus::Result<()>;
    #[zbus(signal)]
    fn device_paired(&self, device_id: String, device: DbusDevice) -> zbus::Result<()>;
    #[zbus(signal)]
    fn device_disconnected(&self, device_id: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn pairing_requested(&self, device_id: String, device_name: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn battery_received(&self, device_id: String, level: i32, is_charging: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    fn connectivity_received(&self, device_id: String, signal_strength: i32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn browse_failed(&self, device_id: String, message: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn update_transfer_progress(&self, progress: u8) -> zbus::Result<()>;
    #[zbus(signal)]
    fn mount_state_changed(&self, device_id: String, mounted: bool) -> zbus::Result<()>;
}
