//! GPU memory per process from NVIDIA's NVML, for drivers that print no DRM `fdinfo` keys (the
//! proprietary `nvidia` driver). `libnvidia-ml.so.1` ships with that driver and is opened with
//! `dlopen`, so other machines pay nothing and no crate wraps it. Only the `--profile` sampler
//! calls in, after it has read the supervisor's own `smaps`.

use std::ffi::{CStr, c_int, c_uint, c_ulonglong, c_void};

type Device = *mut c_void;
type Return = c_int;
type Init = unsafe extern "C" fn() -> Return;
type Shutdown = unsafe extern "C" fn() -> Return;
type DeviceCount = unsafe extern "C" fn(*mut c_uint) -> Return;
type DeviceByIndex = unsafe extern "C" fn(c_uint, *mut Device) -> Return;
type GraphicsProcesses = unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfo) -> Return;

const SUCCESS: Return = 0;
const ERROR_INSUFFICIENT_SIZE: Return = 7;
/// `NVML_VALUE_NOT_AVAILABLE`: the driver withholds `usedGpuMemory`, as under some containers.
const VALUE_NOT_AVAILABLE: c_ulonglong = c_ulonglong::MAX;

/// `nvmlProcessInfo_v2_t`, the layout `nvmlDeviceGetGraphicsRunningProcesses_v3` fills.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfo {
    pid: c_uint,
    used_gpu_memory: c_ulonglong,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

/// The renderer's VRAM in KiB across every NVIDIA device, or `None` without NVML or without a
/// graphics context for `pid`. Loads, queries and unloads each call: kept loaded, NVML and the
/// `libcuda` it pulls in held 20 MiB of supervisor PSS, against 3 MiB left after `nvmlShutdown`
/// and `dlclose`, for about 4 ms of init per sample.
pub(super) fn resident_kib(pid: u32) -> Option<u64> {
    // SAFETY: a NUL-terminated name; the handle is closed below and not used after.
    let handle = unsafe { libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: `handle` is live until the `dlclose` after this call.
    let infos = unsafe { graphics_processes(handle) };
    // SAFETY: nothing resolved from `handle` outlives `graphics_processes`.
    unsafe { libc::dlclose(handle) };
    used_kib(pid, &infos?)
}

/// Sums `pid`'s reported usage, skipping entries the driver withholds. `None` when nothing for
/// `pid` carried a value, so the log never shows a measured-looking zero.
fn used_kib(pid: u32, infos: &[ProcessInfo]) -> Option<u64> {
    infos
        .iter()
        .filter(|info| info.pid == pid && info.used_gpu_memory != VALUE_NOT_AVAILABLE)
        .map(|info| info.used_gpu_memory / 1024)
        .reduce(|a, b| a + b)
}

/// Every device's graphics processes, between `nvmlInit` and `nvmlShutdown`.
unsafe fn graphics_processes(handle: *mut c_void) -> Option<Vec<ProcessInfo>> {
    let symbol = |name: &CStr| {
        // SAFETY: `handle` is live and `name` is NUL-terminated.
        let pointer = unsafe { libc::dlsym(handle, name.as_ptr()) };
        (!pointer.is_null()).then_some(pointer)
    };
    // SAFETY: each symbol is the `nvml.h` function of the transmuted signature.
    let (init, shutdown, device_count, device_by_index, processes) = unsafe {
        (
            std::mem::transmute::<*mut c_void, Init>(symbol(c"nvmlInit_v2")?),
            std::mem::transmute::<*mut c_void, Shutdown>(symbol(c"nvmlShutdown")?),
            std::mem::transmute::<*mut c_void, DeviceCount>(symbol(c"nvmlDeviceGetCount_v2")?),
            std::mem::transmute::<*mut c_void, DeviceByIndex>(symbol(c"nvmlDeviceGetHandleByIndex_v2")?),
            std::mem::transmute::<*mut c_void, GraphicsProcesses>(symbol(c"nvmlDeviceGetGraphicsRunningProcesses_v3")?),
        )
    };
    // SAFETY: the signatures match `nvml.h`; every pointer passed is valid for its call.
    unsafe {
        if init() != SUCCESS {
            return None;
        }
        let mut infos = Vec::new();
        let mut count: c_uint = 0;
        if device_count(&mut count) == SUCCESS {
            for index in 0..count {
                let mut device: Device = std::ptr::null_mut();
                if device_by_index(index, &mut device) == SUCCESS {
                    infos.extend(device_processes(processes, device));
                }
            }
        }
        shutdown();
        Some(infos)
    }
}

/// One device's graphics processes. The list can grow between the sizing call and the read, so
/// the buffer gets slack and one retry; a third miss skips the device for this sample.
unsafe fn device_processes(processes: GraphicsProcesses, device: Device) -> Vec<ProcessInfo> {
    let mut infos = vec![ProcessInfo::default(); 32];
    for _ in 0..2 {
        let mut count = infos.len() as c_uint;
        // SAFETY: `infos` holds `count` elements.
        match unsafe { processes(device, &mut count, infos.as_mut_ptr()) } {
            SUCCESS => {
                infos.truncate(count as usize);
                return infos;
            }
            ERROR_INSUFFICIENT_SIZE => infos.resize(count as usize + 8, ProcessInfo::default()),
            _ => break,
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, used_gpu_memory: u64) -> ProcessInfo {
        ProcessInfo { pid, used_gpu_memory, ..ProcessInfo::default() }
    }

    #[test]
    fn used_kib_sums_the_pid_across_devices_and_skips_withheld_values() {
        let infos = [info(7, 100 << 20), info(8, 999 << 20), info(7, VALUE_NOT_AVAILABLE), info(7, 2 << 20)];
        assert_eq!(used_kib(7, &infos), Some(102 * 1024));
    }

    #[test]
    fn used_kib_is_none_when_the_pid_is_absent_or_every_value_is_withheld() {
        assert_eq!(used_kib(7, &[info(8, 1 << 20)]), None);
        assert_eq!(used_kib(7, &[info(7, VALUE_NOT_AVAILABLE)]), None);
    }

    /// The struct NVML writes into must match `nvmlProcessInfo_v2_t`: u32, pad, u64, u32, u32.
    #[test]
    fn process_info_matches_the_c_layout() {
        assert_eq!(std::mem::size_of::<ProcessInfo>(), 24);
        assert_eq!(std::mem::offset_of!(ProcessInfo, used_gpu_memory), 8);
    }
}
