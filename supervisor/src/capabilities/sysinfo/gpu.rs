//! GPU load, memory and temperature telemetry.

use std::path::Path;

/// Telemetry metrics for the first detected GPU device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

#[derive(serde::Deserialize)]
struct NvtopDevice {
    device_name: Option<String>,
    gpu_util: Option<serde_json::Value>,
    temp: Option<serde_json::Value>,
    mem_used: Option<serde_json::Value>,
    mem_total: Option<serde_json::Value>,
}

fn parse_json_u64(val: Option<&serde_json::Value>) -> Option<u64> {
    match val {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_json_i64(val: Option<&serde_json::Value>) -> Option<i64> {
    match val {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim().strip_suffix('C').unwrap_or(s).trim();
            s.parse::<f64>().ok().map(|f| f.round() as i64)
        }
        _ => None,
    }
}

/// Parses the first GPU entry from `nvtop -s` JSON output.
pub fn parse_nvtop(json_text: &str) -> Option<GpuTelemetry> {
    let list: Vec<NvtopDevice> = serde_json::from_str(json_text).ok()?;
    let first = list.into_iter().next()?;
    let name = first.device_name.unwrap_or_else(|| "GPU".to_string());

    let util_percent = match first.gpu_util.as_ref() {
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim().trim_end_matches('%').trim();
            trimmed.parse::<f64>().ok().map(|f| (f.round() as u8).min(100))
        }
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| (f.round() as u8).min(100)),
        _ => None,
    };

    let temp = parse_json_i64(first.temp.as_ref()).filter(|t| *t > 0);
    let mem_used = parse_json_u64(first.mem_used.as_ref()).filter(|b| *b > 0);
    let mem_total = mem_used.and_then(|_| parse_json_u64(first.mem_total.as_ref()).filter(|b| *b > 0));

    Some(GpuTelemetry { name, util_percent, temp, mem_used, mem_total })
}

/// Parses `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` output.
pub fn parse_nvidia_smi(csv_text: &str) -> Option<GpuTelemetry> {
    let line = csv_text.lines().find(|l| !l.trim().is_empty())?;
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 5 {
        return None;
    }

    let name = parts[0].to_string();
    let util_percent = parts[1].parse::<u8>().ok().map(|u| u.min(100));
    let temp = parts[2].parse::<i64>().ok().filter(|t| *t > 0);
    let mem_used = parts[3].parse::<u64>().ok().and_then(|mib| mib.checked_mul(1024 * 1024));
    let mem_total = parts[4].parse::<u64>().ok().and_then(|mib| mib.checked_mul(1024 * 1024));

    Some(GpuTelemetry { name, util_percent, temp, mem_used, mem_total })
}

/// Reads AMD GPU metrics from DRM sysfs paths when available.
pub fn read_amd_sysfs(drm_device: &Path) -> Option<GpuTelemetry> {
    let busy_str = std::fs::read_to_string(drm_device.join("gpu_busy_percent")).ok()?;
    let util_percent = busy_str.trim().parse::<u8>().ok().map(|u| u.min(100));

    let mem_used =
        std::fs::read_to_string(drm_device.join("mem_info_vram_used")).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let mem_total =
        std::fs::read_to_string(drm_device.join("mem_info_vram_total")).ok().and_then(|s| s.trim().parse::<u64>().ok());

    Some(GpuTelemetry { name: "AMD GPU".to_string(), util_percent, temp: None, mem_used, mem_total })
}

fn find_amd_sysfs(drm_root: &Path) -> Option<GpuTelemetry> {
    let cards = std::fs::read_dir(drm_root).ok()?;
    for card in cards.flatten() {
        let name = card.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.len() == 4 || !name[4..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(gpu) = read_amd_sysfs(&card.path().join("device")) {
            return Some(gpu);
        }
    }
    None
}

/// Samples GPU metrics using available backend tools or sysfs.
pub async fn sample_gpu(drm_root: &Path, fallback_temp: i64) -> Option<GpuTelemetry> {
    // 1. Try nvtop -s: unifies AMD, Intel and NVIDIA.
    if let Ok(Ok(output)) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::process::Command::new("nvtop").arg("-s").kill_on_drop(true).output(),
    )
    .await
        && output.status.success()
        && let Some(mut gpu) = parse_nvtop(&String::from_utf8_lossy(&output.stdout))
    {
        if gpu.temp.is_none() && fallback_temp >= 0 {
            gpu.temp = Some(fallback_temp);
        }
        return Some(gpu);
    }

    // 2. Try nvidia-smi if nvtop is absent.
    if let Ok(Ok(output)) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
        && output.status.success()
        && let Some(gpu) = parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))
    {
        return Some(gpu);
    }

    // 3. Try each DRM card; card0 may be an integrated GPU on hybrid systems.
    if let Some(mut gpu) = find_amd_sysfs(drm_root) {
        if gpu.temp.is_none() && fallback_temp >= 0 {
            gpu.temp = Some(fallback_temp);
        }
        return Some(gpu);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTEL_NVTOP: &str = r#"[{"device_name":"Raptor Lake-P (Iris Xe Graphics)",
        "gpu_util":"19%","temp":null,"mem_total":"33258684416","mem_used":null}]"#;
    const NVIDIA_NVTOP: &str = r#"[{"device_name":"NVIDIA GeForce RTX 3060",
        "gpu_util":"35%","temp":"52","mem_total":"6442450944","mem_used":"1610612736"}]"#;

    #[test]
    fn parse_nvtop_intel_integrated() {
        let gpu = parse_nvtop(INTEL_NVTOP).unwrap();
        assert_eq!(gpu.name, "Raptor Lake-P (Iris Xe Graphics)");
        assert_eq!(gpu.util_percent, Some(19));
        assert_eq!(gpu.temp, None);
        assert_eq!(gpu.mem_used, None);
        assert_eq!(gpu.mem_total, None);
    }

    #[test]
    fn parse_nvtop_nvidia_discrete() {
        let gpu = parse_nvtop(NVIDIA_NVTOP).unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 3060");
        assert_eq!(gpu.util_percent, Some(35));
        assert_eq!(gpu.temp, Some(52));
        assert_eq!(gpu.mem_used, Some(1610612736));
        assert_eq!(gpu.mem_total, Some(6442450944));
    }

    #[test]
    fn parse_nvidia_smi_line() {
        let csv = "NVIDIA GeForce RTX 4070, 42, 58, 2048, 12288\n";
        let gpu = parse_nvidia_smi(csv).unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 4070");
        assert_eq!(gpu.util_percent, Some(42));
        assert_eq!(gpu.temp, Some(58));
        assert_eq!(gpu.mem_used, Some(2048 * 1024 * 1024));
        assert_eq!(gpu.mem_total, Some(12288 * 1024 * 1024));
    }

    #[test]
    fn amd_sysfs_finds_card_after_integrated_gpu() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("card1/device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("gpu_busy_percent"), "42\n").unwrap();
        std::fs::write(device.join("mem_info_vram_used"), "1024\n").unwrap();
        std::fs::write(device.join("mem_info_vram_total"), "2048\n").unwrap();
        let gpu = find_amd_sysfs(dir.path()).unwrap();
        assert_eq!(gpu.util_percent, Some(42));
        assert_eq!(gpu.mem_used, Some(1024));
        assert_eq!(gpu.mem_total, Some(2048));
    }
}
