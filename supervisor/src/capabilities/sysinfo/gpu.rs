//! GPU load, memory and temperature telemetry.

pub use shared::state::sysinfo::GpuTelemetry;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::super::{read_attr, read_parsed};
use super::run_logged;

static NVTOP_BUSY: AtomicBool = AtomicBool::new(false);
static NVIDIA_SMI_BUSY: AtomicBool = AtomicBool::new(false);

#[derive(serde::Deserialize)]
struct NvtopDevice {
    device_name: Option<String>,
    gpu_util: Option<serde_json::Value>,
    temp: Option<serde_json::Value>,
    mem_used: Option<serde_json::Value>,
    mem_total: Option<serde_json::Value>,
}

pub(super) fn parse_json_u64(val: Option<&serde_json::Value>) -> Option<u64> {
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
            let s = s.trim().trim_suffix('C').trim();
            s.parse::<f64>().ok().map(|f| f.round() as i64)
        }
        _ => None,
    }
}

/// Parses `nvtop -s` JSON output. With several devices the first reporting VRAM use wins (a
/// discrete card; an iGPU has none), else the first.
pub fn parse_nvtop(json_text: &str) -> Option<GpuTelemetry> {
    let list: Vec<NvtopDevice> = serde_json::from_str(json_text).ok()?;
    let first = list.into_iter().min_by_key(|d| parse_json_u64(d.mem_used.as_ref()).is_none_or(|b| b == 0))?;
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
    let temp = parts[2].parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f.round() as i64).filter(|t| *t > 0);
    let mem_used = parts[3].parse::<u64>().ok().and_then(|mib| mib.checked_mul(1024 * 1024));
    let mem_total = parts[4].parse::<u64>().ok().and_then(|mib| mib.checked_mul(1024 * 1024));

    Some(GpuTelemetry { name, util_percent, temp, mem_used, mem_total })
}

/// Reads AMD GPU metrics from DRM sysfs paths when available.
pub fn read_amd_sysfs(drm_device: &Path) -> Option<GpuTelemetry> {
    let busy = read_attr(drm_device, "gpu_busy_percent")?;
    let util_percent = busy.parse::<u8>().ok().map(|u| u.min(100));

    let mem_used = read_parsed(drm_device, "mem_info_vram_used");
    let mem_total = read_parsed(drm_device, "mem_info_vram_total");

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

const NVIDIA_SMI_QUERY: &[&str] =
    &["--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total", "--format=csv,noheader,nounits"];

/// Samples GPU metrics using available backend tools or sysfs.
pub async fn sample_gpu(drm_root: &Path, fallback_temp: Option<i64>) -> Option<GpuTelemetry> {
    sample_gpu_with(drm_root, fallback_temp, |program, args, busy| {
        run_logged(program, args, Duration::from_secs(3), busy, true)
    })
    .await
}

async fn sample_gpu_with<F, Fut>(drm_root: &Path, fallback_temp: Option<i64>, mut run: F) -> Option<GpuTelemetry>
where
    F: FnMut(&'static str, &'static [&'static str], &'static AtomicBool) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    // 1. Try nvidia-smi first: ~4x less CPU per sample than nvtop on NVIDIA, and absent elsewhere.
    // Do not apply `fallback_temp` here: `GPU_TEMP_PREFERENCE` resolves `amdgpu` before `nvidia`,
    // which would stamp an iGPU's hwmon reading onto the dGPU when nvidia-smi reports `[N/A]`.
    if let Some(stdout) = run("nvidia-smi", NVIDIA_SMI_QUERY, &NVIDIA_SMI_BUSY).await
        && let Some(gpu) = parse_nvidia_smi(&String::from_utf8_lossy(&stdout))
    {
        return Some(gpu);
    }

    // 2. Try nvtop -s: unifies AMD, Intel and NVIDIA.
    if let Some(stdout) = run("nvtop", &["-s"], &NVTOP_BUSY).await
        && let Some(mut gpu) = parse_nvtop(&String::from_utf8_lossy(&stdout))
    {
        gpu.temp = gpu.temp.or(fallback_temp);
        return Some(gpu);
    }

    // 3. Try each DRM card; card0 may be an integrated GPU on hybrid systems.
    if let Some(mut gpu) = find_amd_sysfs(drm_root) {
        gpu.temp = gpu.temp.or(fallback_temp);
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

    fn write_amd_card(drm_root: &Path) {
        let device = drm_root.join("card1/device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("gpu_busy_percent"), "42\n").unwrap();
        std::fs::write(device.join("mem_info_vram_used"), "1024\n").unwrap();
        std::fs::write(device.join("mem_info_vram_total"), "2048\n").unwrap();
    }

    #[test]
    fn parse_nvtop_reads_intel_and_nvidia_cards_and_prefers_the_discrete_one() {
        let gpu = parse_nvtop(INTEL_NVTOP).unwrap();
        assert_eq!(gpu.name, "Raptor Lake-P (Iris Xe Graphics)");
        assert_eq!(gpu.util_percent, Some(19));
        assert_eq!((gpu.temp, gpu.mem_used, gpu.mem_total), (None, None, None));

        let gpu = parse_nvtop(NVIDIA_NVTOP).unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 3060");
        assert_eq!(gpu.util_percent, Some(35));
        assert_eq!(gpu.temp, Some(52));
        assert_eq!(gpu.mem_used, Some(1610612736));
        assert_eq!(gpu.mem_total, Some(6442450944));

        let both = format!("[{},{}]", &INTEL_NVTOP[1..INTEL_NVTOP.len() - 1], &NVIDIA_NVTOP[1..NVIDIA_NVTOP.len() - 1]);
        assert_eq!(
            parse_nvtop(&both).unwrap().name,
            "NVIDIA GeForce RTX 3060",
            "a leading iGPU loses to the discrete card"
        );
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

        let fractional = "NVIDIA GeForce RTX 3080, 29, 42.53, 2542, 10240\n";
        assert_eq!(parse_nvidia_smi(fractional).unwrap().temp, Some(43));

        for missing_temp in ["[N/A]", "0", "-5", "inf"] {
            let line = format!("NVIDIA GeForce RTX 3080, 29, {missing_temp}, 2542, 10240\n");
            assert_eq!(parse_nvidia_smi(&line).unwrap().temp, None, "{missing_temp}");
        }
    }

    #[test]
    fn amd_sysfs_finds_card_after_integrated_gpu() {
        let dir = tempfile::tempdir().unwrap();
        write_amd_card(dir.path());
        let gpu = find_amd_sysfs(dir.path()).unwrap();
        assert_eq!(gpu.util_percent, Some(42));
        assert_eq!(gpu.mem_used, Some(1024));
        assert_eq!(gpu.mem_total, Some(2048));
    }

    #[tokio::test]
    async fn sample_gpu_tries_nvidia_smi_before_nvtop_then_sysfs_and_applies_fallback_temp() {
        let empty = tempfile::tempdir().unwrap();

        // 1. nvidia-smi success skips nvtop and keeps reported temp over fallback_temp.
        let mut seen = Vec::new();
        let gpu = sample_gpu_with(empty.path(), Some(70), |program, args, _| {
            seen.push((program, args));
            std::future::ready(Some(b"NVIDIA GeForce RTX 3080, 29, 42.53, 2542, 10240\n".to_vec()))
        })
        .await
        .unwrap();
        assert_eq!(seen, vec![("nvidia-smi", NVIDIA_SMI_QUERY)]);
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 3080");
        assert_eq!(gpu.temp, Some(43));

        // 1b. nvidia-smi with missing temp keeps None instead of borrowing an iGPU's fallback_temp.
        let gpu = sample_gpu_with(empty.path(), Some(68), |program, _, _| {
            std::future::ready(match program {
                "nvidia-smi" => Some(b"NVIDIA GeForce RTX 3080, 29, [N/A], 2542, 10240\n".to_vec()),
                _ => unreachable!("nvtop must not run when nvidia-smi succeeds"),
            })
        })
        .await
        .unwrap();
        assert_eq!(gpu.temp, None);

        // 2. nvidia-smi failure or unparseable output falls back to nvtop; null temp uses fallback_temp.
        for bad_smi in [None, Some(Vec::new()), Some(b"not csv\n".to_vec())] {
            let mut calls = Vec::new();
            let mut busies = Vec::new();
            let gpu = sample_gpu_with(empty.path(), Some(61), |program, args, busy| {
                calls.push((program, args));
                busies.push(busy as *const AtomicBool);
                let out = match program {
                    "nvidia-smi" => bad_smi.clone(),
                    "nvtop" => Some(INTEL_NVTOP.as_bytes().to_vec()),
                    other => panic!("unexpected program {other}"),
                };
                std::future::ready(out)
            })
            .await
            .unwrap();
            assert_eq!(calls, vec![("nvidia-smi", NVIDIA_SMI_QUERY), ("nvtop", &["-s"][..])]);
            assert!(!std::ptr::eq(busies[0], busies[1]));
            assert_eq!(gpu.name, "Raptor Lake-P (Iris Xe Graphics)");
            assert_eq!(gpu.temp, Some(61));
        }

        // 3. nvtop with temp keeps it over fallback_temp.
        let gpu = sample_gpu_with(empty.path(), Some(75), |program, _, _| {
            std::future::ready(match program {
                "nvidia-smi" => None,
                "nvtop" => Some(NVIDIA_NVTOP.as_bytes().to_vec()),
                _ => None,
            })
        })
        .await
        .unwrap();
        assert_eq!(gpu.temp, Some(52));

        // 4. Both tools missing/failing falls back to AMD sysfs with fallback_temp.
        let amd_dir = tempfile::tempdir().unwrap();
        write_amd_card(amd_dir.path());
        let mut calls = Vec::new();
        let gpu = sample_gpu_with(amd_dir.path(), Some(55), |program, _, _| {
            calls.push(program);
            std::future::ready(None)
        })
        .await
        .unwrap();
        assert_eq!(calls, vec!["nvidia-smi", "nvtop"]);
        assert_eq!(gpu.name, "AMD GPU");
        assert_eq!(gpu.temp, Some(55));

        // 5. All three backends absent returns None.
        assert_eq!(sample_gpu_with(empty.path(), Some(55), |_, _, _| std::future::ready(None)).await, None);
    }
}
