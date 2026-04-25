use std::fmt::Write as _;

use tokimo_package_ffmpeg::videotoolbox_decode_supported;

use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};

pub(super) fn build_videotoolbox(p: &PipelineParams<'_>) -> PipelineConfig {
    // VT decode when source codec is supported
    let use_hw_decode = p.video_codec.is_some_and(videotoolbox_decode_supported) && p.caps.has_videotoolbox;

    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

    let video_filter = Some(build_videotoolbox_filter(p, use_hw_decode));
    let (decode, filter_backend) = if use_hw_decode && p.caps.has_videotoolbox_full {
        (Some("videotoolbox".to_string()), Some("videotoolbox".to_string()))
    } else {
        (None, None)
    };

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_videotoolbox_hevc {
            "hevc_videotoolbox".to_string()
        } else {
            "h264_videotoolbox".to_string()
        },
        preset: String::new(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_videotoolbox_hevc {
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
        use_cuvid: false,
    }
}

fn build_videotoolbox_filter(p: &PipelineParams<'_>, use_hw_decode: bool) -> String {
    let use_hw_filter = use_hw_decode && p.caps.has_videotoolbox_full;

    // Deinterlace
    let deint = if p.deinterlace {
        if use_hw_filter {
            Some("yadif_videotoolbox=0:-1:0")
        } else if p.caps.has_bwdif {
            Some("bwdif=0:-1:0")
        } else {
            Some("yadif=0:-1:0")
        }
    } else {
        None
    };

    let base = if let Some(tm) = p.tonemap {
        // Jellyfin: scale_vt can do tonemap in one pass when videotoolbox_full
        if use_hw_filter && p.caps.has_videotoolbox_tonemap {
            // scale_vt=tonemap=1:format=yuv420p:p=bt709:t=bt709:m=bt709:…
            let mut args = format!(
                "scale_vt=tonemap=1:format=yuv420p:p=bt709:t=bt709:m=bt709:tonemap={}:peak={}:desat={}",
                tm.algorithm, tm.peak, tm.desat
            );
            if tm.param != 0.0 {
                write!(args, ":param={}", tm.param).unwrap();
            }
            args
        } else {
            // SW tonemap fallback (download first if frames are on GPU)
            build_sw_tonemap(tm, p.caps, false)
        }
    } else if use_hw_filter {
        "scale_vt=format=yuv420p".to_string()
    } else {
        "format=yuv420p".to_string()
    };

    match deint {
        Some(d) => format!("{d},{base}"),
        None => base,
    }
}
