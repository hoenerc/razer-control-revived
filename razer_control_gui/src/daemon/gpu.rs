use std::fs;
use std::path::Path;
use std::process::Command;

use super::comms::GpuInfo;

/// Known GPU vendor IDs
const VENDOR_NVIDIA: &str = "0x10de";
const VENDOR_AMD: &str = "0x1002";
const VENDOR_INTEL: &str = "0x8086";

/// Scan PCI devices and return all detected GPUs
pub fn discover_gpus() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    let pci_dir = Path::new("/sys/bus/pci/devices");

    let entries = match fs::read_dir(pci_dir) {
        Ok(e) => e,
        Err(_) => return gpus,
    };

    for entry in entries.flatten() {
        let dev_path = entry.path();
        let class = read_sysfs_trimmed(&dev_path.join("class"));

        // Check if this is a GPU (VGA controller or 3D controller)
        let is_gpu = match class.as_deref() {
            Some(c) => c.starts_with("0x0300") || c.starts_with("0x0302"),
            None => false,
        };
        if !is_gpu {
            continue;
        }

        let vendor = read_sysfs_trimmed(&dev_path.join("vendor"));
        let device_id = read_sysfs_trimmed(&dev_path.join("device"));
        let pci_slot = entry.file_name().to_string_lossy().to_string();

        // Determine driver from symlink
        let driver = match fs::read_link(dev_path.join("driver")) {
            Ok(link) => link
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        };

        // Determine GPU type based on vendor
        let gpu_type = match vendor.as_deref() {
            Some(VENDOR_NVIDIA) => "dgpu".to_string(),
            Some(VENDOR_AMD) => {
                if driver == "amdgpu" {
                    // On hybrid laptops, AMD is typically the iGPU
                    "igpu".to_string()
                } else {
                    "dgpu".to_string()
                }
            }
            Some(VENDOR_INTEL) => "igpu".to_string(),
            _ => "unknown".to_string(),
        };

        // Get runtime PM status
        let runtime_status = read_sysfs_trimmed(&dev_path.join("power/runtime_status"))
            .unwrap_or_else(|| "unsupported".to_string());

        // Build a human-readable name
        let name = resolve_gpu_name(vendor.as_deref(), device_id.as_deref(), &driver);

        gpus.push(GpuInfo {
            name,
            pci_slot,
            driver,
            gpu_type,
            runtime_status,
        });
    }

    // Sort: iGPU first, then dGPU
    gpus.sort_by(|a, b| a.gpu_type.cmp(&b.gpu_type));
    gpus
}

/// Locate the first dGPU's sysfs device path cheaply, without resolving a name
/// (no nvidia-smi/lspci) — safe to call from a frequent poll loop.
///
/// The result is cached for the daemon's lifetime once found: PCI topology for
/// an internal dGPU is static, and several always-on poll loops call this every
/// couple of seconds — with the cache they cost one HashMap-free Mutex read
/// instead of a /sys/bus/pci/devices directory scan. Only a successful lookup
/// is cached, so a device that appears later (e.g. eGPU) is still discovered.
pub fn find_dgpu_sysfs_path() -> Option<std::path::PathBuf> {
    use std::sync::{Mutex, OnceLock};
    static DGPU_PATH: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
    let cache = DGPU_PATH.get_or_init(|| Mutex::new(None));
    if let Ok(hit) = cache.lock() {
        if hit.is_some() {
            return hit.clone();
        }
    }
    let found = find_dgpu_sysfs_path_uncached();
    if found.is_some() {
        if let Ok(mut slot) = cache.lock() {
            *slot = found.clone();
        }
    }
    found
}

fn find_dgpu_sysfs_path_uncached() -> Option<std::path::PathBuf> {
    let pci_dir = Path::new("/sys/bus/pci/devices");
    for entry in fs::read_dir(pci_dir).ok()?.flatten() {
        let dev_path = entry.path();
        let class = read_sysfs_trimmed(&dev_path.join("class"));
        let is_gpu = matches!(class.as_deref(), Some(c) if c.starts_with("0x0300") || c.starts_with("0x0302"));
        if !is_gpu {
            continue;
        }
        let vendor = read_sysfs_trimmed(&dev_path.join("vendor"));
        let driver = fs::read_link(dev_path.join("driver"))
            .ok()
            .and_then(|link| link.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        // dGPU = NVIDIA, or an AMD GPU not bound to amdgpu (the hybrid iGPU driver)
        let is_dgpu = matches!(vendor.as_deref(), Some(VENDOR_NVIDIA))
            || (matches!(vendor.as_deref(), Some(VENDOR_AMD)) && driver != "amdgpu");
        if is_dgpu {
            return Some(dev_path);
        }
    }
    None
}

/// Check if dGPU runtime PM is set to "auto" (power saving enabled)
pub fn get_dgpu_runtime_pm() -> bool {
    if let Some(dgpu_path) = find_dgpu_sysfs_path() {
        let control = read_sysfs_trimmed(&dgpu_path.join("power/control"));
        matches!(control.as_deref(), Some("auto"))
    } else {
        false
    }
}

/// Read a sysfs file, returning trimmed content
fn read_sysfs_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Resolve a human-readable GPU name from vendor/device IDs and driver.
///
/// Deliberately does NOT use nvidia-smi: querying the NVIDIA driver wakes a
/// runtime-suspended dGPU, and GetGpuStatus is polled by the GUI — a name
/// lookup must never spin the GPU up. lspci only reads kernel-cached PCI
/// config space, which works regardless of the GPU's power state. Results
/// are cached for the daemon's lifetime so the poll spawns no processes.
fn resolve_gpu_name(vendor: Option<&str>, device_id: Option<&str>, driver: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static NAME_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

    let key = format!("{}:{}", vendor.unwrap_or("-"), device_id.unwrap_or("-"));
    let cache = NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(&key) {
            return hit.clone();
        }
    }

    let resolved = resolve_gpu_name_uncached(vendor, device_id, driver);
    if let Ok(mut map) = cache.lock() {
        map.insert(key, resolved.clone());
    }
    resolved
}

fn resolve_gpu_name_uncached(vendor: Option<&str>, device_id: Option<&str>, driver: &str) -> String {
    // Try lspci for a name
    if let Some(dev_id) = device_id {
        // Strip 0x prefix for lspci lookup
        let vid = vendor.unwrap_or("").trim_start_matches("0x");
        let did = dev_id.trim_start_matches("0x");
        if let Ok(output) = Command::new("lspci")
            .args(["-d", &format!("{}:{}", vid, did), "-mm"])
            .output()
        {
            if output.status.success() {
                let line = String::from_utf8_lossy(&output.stdout);
                // lspci -mm format: Slot "Class" "Vendor" "Device" ...
                // Extract the device name (4th quoted field)
                let fields: Vec<&str> = line.split('"').collect();
                if fields.len() >= 8 {
                    let vendor_name = fields[3];
                    let device_name = fields[5];
                    return format!("{} {}", vendor_name, device_name);
                }
            }
        }
    }

    // Fallback: vendor + driver
    let vendor_name = match vendor {
        Some(VENDOR_NVIDIA) => "NVIDIA",
        Some(VENDOR_AMD) => "AMD",
        Some(VENDOR_INTEL) => "Intel",
        _ => "Unknown",
    };
    if driver.is_empty() {
        format!("{} GPU", vendor_name)
    } else {
        format!("{} GPU ({})", vendor_name, driver)
    }
}
