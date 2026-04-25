//! Hardware backend selection and per-backend HLS pipeline configuration.
//!
//! Mirrors Jellyfin's hardware acceleration decision tree:
//! - NVIDIA CUDA (nvenc + cuvid + cuda filters)
//! - Intel QSV (`vpp_qsv` / `scale_qsv`)
//! - VAAPI (`scale_vaapi`, Linux Intel/AMD)
//! - Apple `VideoToolbox` (`scale_vt`, macOS)
//! - AMD AMF (SW decode + amf encode)
//! - Rockchip RKMPP (SW filter)
//! - Pure software (libx264)
//!
//! The low-level FFI pipeline in `tokimo_package_ffmpeg::transcode` already supports all
//! `HwType` variants.  This module only decides *which* backend to use and builds
//! the correct `TranscodeOptions` fields (encoder, filters, decode backend, etc.).

mod amf;
mod cuda;
mod qsv;
mod rkmpp;
pub mod software;
mod vaapi;
mod videotoolbox;

use tokimo_package_ffmpeg::HwCapabilities;

use crate::types::TonemapOptions;

// ── Backend selection ────────────────────────────────────────────────────────

/// The hardware (or software) acceleration backend chosen for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwBackend {
    /// NVIDIA: CUVID decode (when src codec supported) + `scale_cuda` + `h264_nvenc` or `hevc_nvenc`.
    Cuda,
    /// Apple `VideoToolbox`: VT decode + `scale_vt` + `h264_videotoolbox` or `hevc_videotoolbox` (macOS only).
    VideoToolbox,
    /// VAAPI: vaapi decode + `scale_vaapi` + `h264_vaapi` or `hevc_vaapi` (Linux Intel/AMD).
    Vaapi,
    /// Intel Quick Sync: QSV decode + `vpp_qsv` + `h264_qsv` or `hevc_qsv`.
    Qsv,
    /// AMD AMF: SW/d3d11va decode + SW filters + `h264_amf` or `hevc_amf`.
    Amf,
    /// Rockchip RKMPP: rkmpp decode + SW filters + `h264_rkmpp` or `hevc_rkmpp`.
    Rkmpp,
    /// Pure software: libx264 or libx265.
    Software,
}

impl HwBackend {
    pub fn label(self) -> &'static str {
        match self {
            HwBackend::Cuda => "cuda",
            HwBackend::VideoToolbox => "videotoolbox",
            HwBackend::Vaapi => "vaapi",
            HwBackend::Qsv => "qsv",
            HwBackend::Amf => "amf",
            HwBackend::Rkmpp => "rkmpp",
            HwBackend::Software => "software",
        }
    }

    /// Whether this backend uses a HW encoder (not libx264).
    pub fn is_hw_encoder(self) -> bool {
        !matches!(self, HwBackend::Software)
    }
}

/// Select the best available backend.
///
/// Priority (matches Jellyfin auto-detect order):
/// NVIDIA > `VideoToolbox` (macOS) > VAAPI > QSV > AMF > RKMPP > software
pub fn select_backend(caps: &HwCapabilities) -> HwBackend {
    if caps.has_nvenc {
        return HwBackend::Cuda;
    }
    if caps.has_videotoolbox {
        return HwBackend::VideoToolbox;
    }
    if caps.has_vaapi {
        return HwBackend::Vaapi;
    }
    if caps.has_qsv {
        return HwBackend::Qsv;
    }
    if caps.has_amf {
        return HwBackend::Amf;
    }
    if caps.has_rkmpp {
        return HwBackend::Rkmpp;
    }
    HwBackend::Software
}

// ── Pipeline configuration ───────────────────────────────────────────────────

/// All fields needed to configure the `FFmpeg` encode/filter pipeline.
/// Returned by per-backend builder functions and consumed by `ffmpeg.rs`.
pub struct PipelineConfig {
    /// Video encoder name (e.g. "`h264_nvenc`", "`h264_vaapi`", "libx264").
    pub encoder: String,
    /// `FFmpeg` -preset value.  Empty string = omit -preset.
    pub preset: String,
    /// CBR target bitrate (e.g. "1716k").
    pub bitrate: Option<String>,
    /// CBR max bitrate.
    pub maxrate: Option<String>,
    /// VBV buffer size.
    pub bufsize: Option<String>,
    /// CRF (SW only).
    pub crf: Option<u32>,
    /// Video encoding profile (e.g. "high").
    pub profile: Option<String>,
    /// Force-keyframe interval (SW only, for segment boundaries).
    pub force_kf: Option<f64>,
    /// GOP size in frames.
    pub gop: Option<i32>,
    /// `Keyint_min`.
    pub keyint_min: Option<i32>,
    /// HW decode backend type passed to `resolve_pipeline` (e.g. "cuda", "vaapi").
    /// None = software decode.
    pub decode: Option<String>,
    /// Filter backend type ("cuda", "vaapi", "qsv", …).  None = software.
    pub filter_backend: Option<String>,
    /// Assembled video filter string (e.g. "`scale_cuda=format=yuv420p`").
    pub video_filter: Option<String>,
    /// True when CUVID named decoder is used (for session logging).
    pub use_cuvid: bool,
}

// ── Per-backend builders ─────────────────────────────────────────────────────

pub struct PipelineParams<'a> {
    pub caps: &'a HwCapabilities,
    pub backend: HwBackend,
    pub video_codec: Option<&'a str>,
    pub video_fps: Option<f64>,
    pub video_bitrate: Option<u64>,
    pub segment_duration: u32,
    pub deinterlace: bool,
    pub tonemap: Option<&'a TonemapOptions>,
    pub out_bitrate_kbps: u64,
    /// When true, prefer HEVC output over H.264 (if the selected backend supports it).
    /// Falls back to H.264 automatically when the HEVC encoder is unavailable.
    pub use_hevc: bool,
}

/// Build the full `PipelineConfig` for the selected backend.
pub fn build_pipeline(p: &PipelineParams<'_>) -> PipelineConfig {
    match p.backend {
        HwBackend::Cuda => cuda::build_cuda(p),
        HwBackend::VideoToolbox => videotoolbox::build_videotoolbox(p),
        HwBackend::Vaapi => vaapi::build_vaapi(p),
        HwBackend::Qsv => qsv::build_qsv(p),
        HwBackend::Amf => amf::build_amf(p),
        HwBackend::Rkmpp => rkmpp::build_rkmpp(p),
        HwBackend::Software => software::build_software(p),
    }
}

// Re-export public items from backend modules
pub use cuda::build_cuda_tonemap;
pub use software::{best_audio_encoder, build_sw_tonemap};
