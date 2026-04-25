use std::fmt::Write as _;

use tokimo_package_ffmpeg::vaapi_decode_supported;

use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};

pub(super) fn build_vaapi(p: &PipelineParams<'_>) -> PipelineConfig {
    let use_hw_decode = p.video_codec.is_some_and(vaapi_decode_supported) && p.caps.has_vaapi;

    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

    let video_filter = Some(build_vaapi_filter(p, use_hw_decode));
    let (decode, filter_backend) = if use_hw_decode && p.caps.has_vaapi_full {
        (Some("vaapi".to_string()), Some("vaapi".to_string()))
    } else {
        // SW decode → hwupload needed before filter.
        // We pass filter_backend=vaapi but no decode, letting the pipeline do hwupload.
        (
            None,
            if p.caps.has_vaapi_full {
                Some("vaapi".to_string())
            } else {
                None
            },
        )
    };

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_vaapi_hevc {
            "hevc_vaapi".to_string()
        } else {
            "h264_vaapi".to_string()
        },
        preset: String::new(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_vaapi_hevc {
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

fn build_vaapi_filter(p: &PipelineParams<'_>, use_hw_decode: bool) -> String {
    let use_hw_filter = p.caps.has_vaapi_full;

    let deint = if p.deinterlace {
        if use_hw_filter {
            Some("deinterlace_vaapi")
        } else if p.caps.has_bwdif {
            Some("bwdif=0:-1:0")
        } else {
            Some("yadif=0:-1:0")
        }
    } else {
        None
    };

    let base = if let Some(tm) = p.tonemap {
        if use_hw_filter {
            // Jellyfin: tonemap_vaapi for Gen9+ Intel / AMD with full VAAPI support
            let mut args = format!(
                "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:tonemap={}:peak={}:desat={}",
                tm.algorithm, tm.peak, tm.desat
            );
            if tm.param != 0.0 {
                write!(args, ":param={}", tm.param).unwrap();
            }
            format!("setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,{args}")
        } else {
            // SW fallback — download if frames are on GPU
            build_sw_tonemap(tm, p.caps, use_hw_decode)
        }
    } else if use_hw_filter {
        "scale_vaapi=format=nv12".to_string()
    } else {
        "format=yuv420p".to_string()
    };

    match deint {
        Some(d) => format!("{d},{base}"),
        None => base,
    }
}
