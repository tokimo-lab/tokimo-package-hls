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
/// 2. `bin/ffmpeg/ffmpeg` relative to workspace root
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
                    let candidate = ancestor.join("bin/ffmpeg/ffmpeg");
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
fn amd_gpu_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Ok(rd) = std::fs::read_dir("/sys/bus/pci/drivers/amdgpu") {
            return rd.count() > 0;
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Apply runtime hardware-presence gates to capabilities detected via FFI.
///
/// FFmpeg's internal registry only reflects what was compiled in, not what
/// hardware is installed. This pass zeros out backends whose GPU is absent.
/// NVIDIA (nvenc/cuvid/cuda) is trusted from the FFI check — the CUDA runtime
/// library is only loadable when NVIDIA drivers are installed, so false
/// positives are not a concern in practice.
fn apply_hw_presence_gates(caps: &mut HwCapabilities) {
    let has_intel = intel_gpu_present();
    let has_amd = amd_gpu_present();

    // VAAPI on Linux: requires Intel or AMD GPU.
    // nvidia-vaapi-driver is detected separately via its driver library.
    #[cfg(target_os = "linux")]
    if !has_intel && !has_amd {
        let has_nvvaapi = [
            "/usr/lib/x86_64-linux-gnu/dri/nvidia_drv_video.so",
            "/usr/lib64/dri/nvidia_drv_video.so",
            "/usr/local/lib/dri/nvidia_drv_video.so",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists());

        if !has_nvvaapi {
            caps.has_vaapi = false;
            caps.has_vaapi_full = false;
            caps.has_vaapi_hevc = false;
        }
    }

    // QSV requires Intel GPU.
    if !has_intel {
        caps.has_qsv = false;
        caps.has_qsv_full = false;
        caps.has_qsv_hevc = false;
    }

    // AMF requires AMD GPU (on Linux; Windows D3D12 path keeps FFI result).
    #[cfg(target_os = "linux")]
    if !has_amd {
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
