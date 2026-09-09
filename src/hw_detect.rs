//! Hardware acceleration detection — thin wrapper over `tokimo_package_ffmpeg::capabilities`.
//!
//! Two-phase detection:
//! 1. FFI registry check — which codecs/filters are compiled into FFmpeg.
//! 2. Runtime hardware presence check — whether the actual GPU/device exists on
//!    this machine, using OS device nodes and driver entries. This prevents false
//!    positives when FFmpeg is compiled with support for a backend (e.g. VAAPI,
//!    QSV) but no matching hardware is installed.
//! 3. CUVID per-codec hardware probe — for NVIDIA GPUs, actually tests each
//!    CUVID decoder via `avcodec_open2` to detect which codecs the GPU supports
//!    in hardware (e.g. AV1 NVDEC requires Ampere+; Turing only does H.264/HEVC).

#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::info;

// Re-export the FFI-based type directly so downstream code is unchanged.
pub use tokimo_package_ffmpeg::capabilities::HwCapabilities;

/// Cached hardware capabilities (populated once on first access).
static HW_CAPS: OnceLock<HwCapabilities> = OnceLock::new();

/// Cached `FFmpeg` binary path (populated once on first access).
static FFMPEG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Resolve the `FFmpeg` binary path.
///
/// Search order:
/// 1. `FFMPEG_BIN` / `FFMPEG_LOCATION` env vars
/// 2. `bin/tokimo-lib/current/bin/ffmpeg` relative to workspace root
/// 3. System `PATH`
pub fn resolve_ffmpeg_binary() -> PathBuf {
    FFMPEG_PATH
        .get_or_init(|| {
            for key in ["FFMPEG_BIN", "FFMPEG_LOCATION"] {
                if let Ok(val) = std::env::var(key) {
                    let p = PathBuf::from(&val);
                    if p.is_file() {
                        info!("[HW] FFmpeg from env ${key}: {}", p.display());
                        return p;
                    }
                    if p.is_dir() {
                        for sub in ["ffmpeg", "bin/ffmpeg"] {
                            let candidate = p.join(sub);
                            if candidate.is_file() {
                                info!("[HW] FFmpeg from env ${key}: {}", candidate.display());
                                return candidate;
                            }
                        }
                    }
                }
            }

            if let Ok(cwd) = std::env::current_dir() {
                for ancestor in cwd.ancestors() {
                    let candidate = ancestor.join("bin/tokimo-lib/current/bin/ffmpeg");
                    if candidate.is_file() {
                        info!("[HW] FFmpeg from workspace: {}", candidate.display());
                        return candidate;
                    }
                }
            }

            info!("[HW] FFmpeg: falling back to system PATH");
            PathBuf::from("ffmpeg")
        })
        .clone()
}

// ── Runtime hardware presence (Linux) ────────────────────────────────────────

/// Returns true if an Intel GPU (iGPU or discrete) is present.
/// Checks for the `i915` or `xe` DRM kernel driver on Linux.
fn intel_gpu_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        for driver in ["i915", "xe"] {
            let path = format!("/sys/bus/pci/drivers/{driver}");
            if let Ok(rd) = std::fs::read_dir(&path)
                && rd.count() > 0
            {
                return true;
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Returns true if an AMD GPU is present (`amdgpu` driver on Linux).
#[cfg(target_os = "linux")]
fn amd_gpu_present() -> bool {
    if let Ok(rd) = std::fs::read_dir("/sys/bus/pci/drivers/amdgpu") {
        return rd.count() > 0;
    }
    false
}

#[cfg(any(target_os = "linux", test))]
fn is_nvidia_gpu_device_name(name: &str) -> bool {
    name.strip_prefix("nvidia")
        .is_some_and(|index| !index.is_empty() && index.chars().all(|character| character.is_ascii_digit()))
}

/// Returns true when the process can access the NVIDIA control device and at
/// least one GPU device. `/proc/driver/nvidia` may be inherited from the host
/// by an ordinary container even when NVIDIA devices were not passed through.
#[cfg(target_os = "linux")]
fn nvidia_gpu_present() -> bool {
    Path::new("/dev/nvidiactl").exists()
        && std::fs::read_dir("/dev").is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_str().is_some_and(is_nvidia_gpu_device_name))
        })
}

#[cfg(any(target_os = "linux", test))]
fn is_dri_render_device_name(name: &str) -> bool {
    name.strip_prefix("renderD")
        .is_some_and(|index| !index.is_empty() && index.chars().all(|character| character.is_ascii_digit()))
}

/// Returns true when a DRM render node is available inside this process's
/// device namespace. Host PCI driver entries alone do not grant a container
/// access to VAAPI, QSV, or AMF devices.
#[cfg(target_os = "linux")]
fn dri_render_device_present() -> bool {
    std::fs::read_dir("/dev/dri").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_str().is_some_and(is_dri_render_device_name))
    })
}

/// Apply runtime hardware-presence gates to capabilities detected via FFI.
///
/// FFmpeg's internal registry only reflects what was compiled in, not what
/// hardware is installed. This pass zeros out backends whose GPU is absent.
fn apply_hw_presence_gates(caps: &mut HwCapabilities) {
    let has_intel = intel_gpu_present();
    #[cfg(target_os = "linux")]
    let has_amd = amd_gpu_present();
    #[cfg(target_os = "linux")]
    let has_dri_render_device = dri_render_device_present();
    #[cfg(target_os = "linux")]
    let has_intel_device_access = has_intel && has_dri_render_device;
    #[cfg(not(target_os = "linux"))]
    let has_intel_device_access = has_intel;

    // FFmpeg registry discovery only proves that CUDA support was compiled in.
    // Containers without NVIDIA device passthrough still see those codecs and
    // can even inherit /proc/driver/nvidia from the host.
    #[cfg(target_os = "linux")]
    if !nvidia_gpu_present() {
        caps.has_nvenc = false;
        caps.has_nvenc_hevc = false;
        caps.has_cuvid = false;
        caps.has_cuda_full = false;
        caps.has_bwdif_cuda = false;
        caps.cuvid_hw_codecs.clear();
    }

    // VAAPI on Linux: requires Intel or AMD GPU.
    // nvidia-vaapi-driver is detected separately via its driver library.
    #[cfg(target_os = "linux")]
    {
        let has_nvvaapi = [
            "/usr/lib/x86_64-linux-gnu/dri/nvidia_drv_video.so",
            "/usr/lib64/dri/nvidia_drv_video.so",
            "/usr/local/lib/dri/nvidia_drv_video.so",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists());

        if !has_dri_render_device || (!has_intel && !has_amd && !has_nvvaapi) {
            caps.has_vaapi = false;
            caps.has_vaapi_full = false;
            caps.has_vaapi_hevc = false;
        }
    }

    // QSV requires Intel GPU.
    if !has_intel_device_access {
        caps.has_qsv = false;
        caps.has_qsv_full = false;
        caps.has_qsv_hevc = false;
    }

    // AMF requires AMD GPU (on Linux; Windows D3D12 path keeps FFI result).
    #[cfg(target_os = "linux")]
    if !has_amd || !has_dri_render_device {
        caps.has_amf = false;
        caps.has_amf_hevc = false;
    }

    // RKMPP: Rockchip SoC only.
    #[cfg(target_os = "linux")]
    if !std::path::Path::new("/dev/rga").exists() && !std::path::Path::new("/dev/mpp_service").exists() {
        caps.has_rkmpp = false;
        caps.has_rkmpp_hevc = false;
    }

    // VideoToolbox: macOS only.
    #[cfg(not(target_os = "macos"))]
    {
        caps.has_videotoolbox = false;
        caps.has_videotoolbox_full = false;
        caps.has_videotoolbox_hevc = false;
        caps.has_videotoolbox_tonemap = false;
    }
}

/// Get (or lazily detect) hardware capabilities.
///
/// Phase 1: FFI registry query (zero subprocess overhead).
/// Phase 2: Runtime hardware-presence gates (OS device/driver checks).
/// Phase 3: CUVID per-codec hardware probe (if NVIDIA GPU is present).
pub fn get_hw_capabilities() -> &'static HwCapabilities {
    HW_CAPS.get_or_init(|| {
        let mut caps = tokimo_package_ffmpeg::capabilities::detect_capabilities().clone();
        apply_hw_presence_gates(&mut caps);
        if caps.has_cuvid && caps.has_cuda_full {
            let probed = tokimo_package_ffmpeg::probe_cuvid_hw_codecs();
            let codec_list: Vec<&str> = probed.iter().map(String::as_str).collect();
            info!("[HW] CUVID hardware probe: {:?}", codec_list);
            caps.cuvid_hw_codecs = probed;
        }
        caps
    })
}

/// Get the CUVID decoder name for the given source video codec.
/// Delegates to `tokimo_package_ffmpeg::capabilities::get_cuvid_decoder`.
pub use tokimo_package_ffmpeg::capabilities::get_cuvid_decoder;

#[cfg(test)]
mod tests {
    use super::{is_dri_render_device_name, is_nvidia_gpu_device_name};

    #[test]
    fn identifies_only_numbered_nvidia_gpu_nodes() {
        assert!(is_nvidia_gpu_device_name("nvidia0"));
        assert!(is_nvidia_gpu_device_name("nvidia12"));
        assert!(!is_nvidia_gpu_device_name("nvidiactl"));
        assert!(!is_nvidia_gpu_device_name("nvidia-uvm"));
        assert!(!is_nvidia_gpu_device_name("nvidia"));
    }

    #[test]
    fn identifies_only_drm_render_nodes() {
        assert!(is_dri_render_device_name("renderD128"));
        assert!(is_dri_render_device_name("renderD129"));
        assert!(!is_dri_render_device_name("card0"));
        assert!(!is_dri_render_device_name("renderD"));
        assert!(!is_dri_render_device_name("renderDfoo"));
    }
}
