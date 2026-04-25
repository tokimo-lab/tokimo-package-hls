use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};

pub(super) fn build_rkmpp(p: &PipelineParams<'_>) -> PipelineConfig {
    // RKMPP: HW decode + SW filter (no GPU filter chain) + HW encode.
    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

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

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_rkmpp_hevc {
            "hevc_rkmpp".to_string()
        } else {
            "h264_rkmpp".to_string()
        },
        preset: String::new(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_rkmpp_hevc {
            Some("main".to_string())
        } else {
            Some("high".to_string())
        },
        force_kf: None,
        gop: Some(gop),
        keyint_min: Some(gop),
        decode: Some("rkmpp".to_string()),
        filter_backend: None,
        video_filter: Some(format!("{deint}{base}")),
        use_cuvid: false,
    }
}
