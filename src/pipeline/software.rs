use std::fmt::Write as _;

use tokimo_package_ffmpeg::{HwCapabilities, best_aac_encoder};

use super::{PipelineConfig, PipelineParams};
use crate::types::TonemapOptions;

pub(super) fn build_software(p: &PipelineParams<'_>) -> PipelineConfig {
    let deint = if p.deinterlace {
        if p.caps.has_bwdif {
            "bwdif=0:-1:0,"
        } else {
            "yadif=0:-1:0,"
        }
    } else {
        ""
    };
    let base = if let Some(tm) = p.tonemap {
        build_sw_tonemap(tm, p.caps, false)
    } else {
        "format=yuv420p".to_string()
    };

    let use_hevc_sw = p.use_hevc && p.caps.has_libx265;
    PipelineConfig {
        encoder: if use_hevc_sw {
            "libx265".to_string()
        } else {
            "libx264".to_string()
        },
        preset: "veryfast".to_string(),
        bitrate: None,
        maxrate: None,
        bufsize: None,
        // libx265 CRF 28 ≈ libx264 CRF 23 visually (same ~40% bitrate saving as HEVC).
        crf: if use_hevc_sw { Some(28) } else { Some(23) },
        profile: if use_hevc_sw {
            Some("main".to_string())
        } else {
            Some("high".to_string())
        },
        force_kf: Some(f64::from(p.segment_duration)),
        gop: None,
        keyint_min: None,
        decode: None,
        filter_backend: None,
        video_filter: Some(format!("{deint}{base}")),
        use_cuvid: false,
    }
}

/// Software HDR→SDR tone mapping filter chain.
/// Matches Jellyfin's `GetSwVidFilterChain`:
/// 1. tonemapx (`FFmpeg` 7+, preferred)
/// 2. zscale + tonemap (older SW chain)
/// 3. tonemap only (last resort)
///
/// `frames_on_gpu`: when true, prepend hwdownload (e.g. CUVID/VAAPI frames).
pub fn build_sw_tonemap(tm: &TonemapOptions, caps: &HwCapabilities, frames_on_gpu: bool) -> String {
    let mut parts: Vec<String> = Vec::new();

    if frames_on_gpu {
        parts.push("hwdownload".to_string());
        parts.push("format=p010le".to_string());
    }

    if caps.has_tonemapx {
        let mut args = format!(
            "tonemapx=tonemap={}:desat={}:peak={}:t=bt709:m=bt709:p=bt709:format=yuv420p",
            tm.algorithm, tm.desat, tm.peak
        );
        if tm.param != 0.0 {
            write!(args, ":param={}", tm.param).unwrap();
        }
        if tm.range == "tv" || tm.range == "pc" {
            write!(args, ":range={}", tm.range).unwrap();
        }
        parts.push(args);
    } else if caps.has_zscale {
        parts.push("setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc".to_string());
        parts.push(format!("zscale=t=linear:npl={}", tm.peak));
        parts.push("format=gbrpf32le".to_string());
        parts.push(format!("tonemap={}:desat={}:peak={}", tm.algorithm, tm.desat, tm.peak));
        parts.push("zscale=t=bt709:m=bt709:p=bt709:r=tv".to_string());
        parts.push("format=yuv420p".to_string());
    } else {
        // Last resort: raw tonemap filter
        parts.push("format=gbrpf32le".to_string());
        parts.push(format!("tonemap={}:desat={}:peak={}", tm.algorithm, tm.desat, tm.peak));
        parts.push("format=yuv420p".to_string());
        parts.push("setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_string());
    }

    parts.join(",")
}

/// Return the best AAC encoder for audio transcoding.
pub fn best_audio_encoder() -> &'static str {
    best_aac_encoder()
}
