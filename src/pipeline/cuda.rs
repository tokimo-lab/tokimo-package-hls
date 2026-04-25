use std::fmt::Write as _;

use tokimo_package_ffmpeg::{HwCapabilities, get_cuvid_decoder};

use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};
use crate::types::TonemapOptions;

pub(super) fn build_cuda(p: &PipelineParams<'_>) -> PipelineConfig {
    let cuvid_decoder = p.video_codec.and_then(|vc| get_cuvid_decoder(vc, p.caps));
    let use_cuvid = p.caps.has_cuda_full && cuvid_decoder.is_some();

    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

    let video_filter = Some(build_cuda_filter(p, use_cuvid));
    let (decode, filter_backend) = if use_cuvid {
        (Some("cuda".to_string()), Some("cuda".to_string()))
    } else {
        (None, None)
    };

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_nvenc_hevc {
            "hevc_nvenc".to_string()
        } else {
            "h264_nvenc".to_string()
        },
        preset: "p1".to_string(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_nvenc_hevc {
            Some("main".to_string())
        } else {
            Some("high".to_string())
        },
        force_kf: None,
        gop: Some(gop),
        keyint_min: Some(gop),
        decode,
        filter_backend,
        video_filter,
        use_cuvid,
    }
}

fn build_cuda_filter(p: &PipelineParams<'_>, use_cuvid: bool) -> String {
    let deint = if p.deinterlace {
        if use_cuvid && p.caps.has_cuda_full {
            if p.caps.has_bwdif_cuda {
                Some("bwdif_cuda=0:-1:0")
            } else {
                Some("yadif_cuda=0:-1:0")
            }
        } else if p.caps.has_bwdif {
            Some("bwdif=0:-1:0")
        } else {
            Some("yadif=0:-1:0")
        }
    } else {
        None
    };

    let base = if let Some(tm) = p.tonemap {
        build_cuda_tonemap(tm, p.caps, use_cuvid)
    } else {
        "format=yuv420p".to_string()
    };

    match deint {
        Some(d) => format!("{d},{base}"),
        None => base,
    }
}

/// CUDA HDR→SDR tonemap.  Matches Jellyfin `GetNvidiaVidFiltersPrefered`.
pub fn build_cuda_tonemap(tm: &TonemapOptions, caps: &HwCapabilities, use_cuvid: bool) -> String {
    // Path 1: Full CUDA (GPU→GPU, no hwdownload needed)
    if caps.has_cuda_full && use_cuvid {
        let mut args = format!(
            "tonemap_cuda=format=yuv420p:p=bt709:t=bt709:m=bt709:tonemap={}:peak={}:desat={}",
            tm.algorithm, tm.peak, tm.desat
        );
        if tm.mode != "auto" && !tm.mode.is_empty() {
            write!(args, ":tonemap_mode={}", tm.mode).unwrap();
        }
        if tm.param != 0.0 {
            write!(args, ":param={}", tm.param).unwrap();
        }
        if tm.range == "tv" || tm.range == "pc" {
            write!(args, ":range={}", tm.range).unwrap();
        }
        return format!("setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,{args}");
    }

    // Path 2/3: SW tonemap (with optional hwdownload if frames are on GPU)
    build_sw_tonemap(tm, caps, use_cuvid)
}
