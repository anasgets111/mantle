//! `mantle.sysinfo` snapshot payload.

use serde::Serialize;

/// `mantle.sysinfo`'s payload; `nil` until `configure` sets an interval and a reading changes a field.
/// Pushes only on a change (ADR-0035).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SysinfoState {
    /// CPU utilization across all cores, `0` to `100`, rounded down; `0` until two samples form a delta.
    pub cpu_percent: u8,
    /// Physical memory in use (`MemTotal - MemAvailable`), `0` to `100`, rounded down.
    pub ram_percent: u8,
    /// Swap in use, `0` to `100`, rounded down; also `0` without swap.
    pub swap_percent: u8,
    /// CPU temperatures in whole Celsius: per core (`coretemp`) or per CCD (`k10temp`), else one
    /// package or `acpitz` reading; empty without a sensor. An unreadable sensor is skipped.
    pub temp_cores: Vec<i64>,
    /// `amdgpu`, `nouveau`, `nvidia`, `i915` or `xe` hwmon temperature in whole Celsius, or `-1` without a
    /// readable one.
    pub temp_gpu: i64,
    /// Physical block devices and their mounted partitions.
    pub disks: Vec<DiskDevice>,
    /// GPU telemetry (load, VRAM, temperature) if a supported backend was detected; `nil` otherwise.
    pub gpu: Option<GpuTelemetry>,
    /// Download rate across active non-loopback interfaces in bytes per second; 0 until two samples form a delta.
    pub net_rx_bytes_sec: u64,
    /// Upload rate across active non-loopback interfaces in bytes per second; 0 until two samples form a delta.
    pub net_tx_bytes_sec: u64,
}

impl Default for SysinfoState {
    /// Pre-first-sample sentinels (ADR-0035, ADR-0282): `0` for the three percent fields; `temp_gpu` uses
    /// its IDL-mandated `-1`; `disks` empty; `gpu` None; net rates 0.
    fn default() -> Self {
        Self {
            cpu_percent: 0,
            ram_percent: 0,
            swap_percent: 0,
            temp_cores: Vec::new(),
            temp_gpu: -1,
            disks: Vec::new(),
            gpu: None,
            net_rx_bytes_sec: 0,
            net_tx_bytes_sec: 0,
        }
    }
}

/// One mounted partition under a physical block device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiskPartition {
    /// Mountpoint path, e.g. "/" or "/home".
    pub mount_point: String,
    /// Bytes in use on this filesystem.
    pub used_bytes: u64,
    /// Total bytes on this filesystem.
    pub total_bytes: u64,
    /// Percentage in use, 0 to 100.
    pub percent: u8,
}

/// One physical block device and its mounted partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiskDevice {
    /// Kernel block device name, e.g. "nvme0n1" or "sda".
    pub name: String,
    /// Aggregate bytes in use across mounted partitions.
    pub used_bytes: u64,
    /// Aggregate total bytes across mounted partitions.
    pub total_bytes: u64,
    /// Aggregate percentage in use, 0 to 100.
    pub percent: u8,
    /// Mounted partitions under this block device.
    pub partitions: Vec<DiskPartition>,
}

/// Telemetry metrics for the first detected GPU device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GpuTelemetry {
    /// Device model name reported by driver/tool, e.g. "Raptor Lake-P (Iris Xe Graphics)" or "NVIDIA GeForce RTX 4070".
    pub name: String,
    /// GPU core utilization percentage, 0 to 100.
    pub util_percent: Option<u8>,
    /// GPU temperature in whole Celsius, or `nil` if no sensor reported.
    pub temp: Option<i64>,
    /// Dedicated/used VRAM in bytes, or `nil` if shared or unavailable.
    pub mem_used: Option<u64>,
    /// Total VRAM in bytes, or `nil` if shared or unavailable.
    pub mem_total: Option<u64>,
}
