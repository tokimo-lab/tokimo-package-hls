use tokimo_package_ffmpeg::qsv_decode_supported;

use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};

pub(super) fn build_qsv(p: &PipelineParams<'_>) -> PipelineConfig {
    let use_hw_decode = p.video_codec.is_some_and(qsv_decode_supported) && p.caps.has_qsv;

    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

    let video_filter = Some(build_qsv_filter(p, use_hw_decode));
    let (decode, filter_backend) = if use_hw_decode && p.caps.has_qsv_full {
        (Some("qsv".to_string()), Some("qsv".to_string()))
    } else {
        (
            None,
            if p.caps.has_qsv_full {
                Some("qsv".to_string())
            } else {
                None
            },
        )
    };

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_qsv_hevc {
            "hevc_qsv".to_string()
        } else {
            "h264_qsv".to_string()
        },
        preset: "veryfast".to_string(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_qsv_hevc {
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

fn build_qsv_filter(p: &PipelineParams<'_>, use_hw_decode: bool) -> String {
    let use_hw_filter = p.caps.has_qsv_full;

    let deint = if p.deinterlace {
        if use_hw_filter {
            // Jellyfin: yadif_qsv for QSV pipelines
            Some("yadif_qsv=0:-1:0")
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
            // Jellyfin: vpp_qsv tonemap for Gen12+ (TGL+), requires VPL
            "vpp_qsv=tonemap=1:format=nv12:async_depth=2".to_string()
        } else {
            build_sw_tonemap(tm, p.caps, use_hw_decode)
        }
    } else if use_hw_filter {
        // Jellyfin: prefer scale_qsv for unified QSV pipeline
        "scale_qsv=format=nv12".to_string()
    } else {
        "format=yuv420p".to_string()
    };

    match deint {
        Some(d) => format!("{d},{base}"),
        None => base,
    }
}
