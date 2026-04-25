use tokimo_package_ffmpeg::amf_decode_supported;

use super::software::build_sw_tonemap;
use super::{PipelineConfig, PipelineParams};

pub(super) fn build_amf(p: &PipelineParams<'_>) -> PipelineConfig {
    // AMF: SW decode (or d3d11va on Windows) + SW filters + AMF encode.
    // No native GPU filter chain — download to CPU before filtering.
    let br = format!("{}k", p.out_bitrate_kbps);
    let buf = format!("{}k", p.out_bitrate_kbps * 2);
    let fps = p.video_fps.unwrap_or(24.0);
    let gop = (f64::from(p.segment_duration) * fps).ceil() as i32;

    // Jellyfin: AMF on Windows uses d3d11va for decode (hwaccel only, no named decoder).
    // On Linux this is uncommon — use SW decode.
    let use_d3d11va = cfg!(target_os = "windows") && p.video_codec.is_some_and(amf_decode_supported);

    let decode = if use_d3d11va { Some("d3d11va".to_string()) } else { None };

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

    let video_filter = Some(format!("{deint}{base}"));

    PipelineConfig {
        encoder: if p.use_hevc && p.caps.has_amf_hevc {
            "hevc_amf".to_string()
        } else {
            "h264_amf".to_string()
        },
        preset: String::new(),
        bitrate: Some(br.clone()),
        maxrate: Some(br),
        bufsize: Some(buf),
        crf: None,
        profile: if p.use_hevc && p.caps.has_amf_hevc {
            Some("main".to_string())
        } else {
            Some("high".to_string())
        },
        force_kf: None,
        gop: Some(gop),
        keyint_min: Some(gop),
        decode,
        filter_backend: None, // SW filters
        video_filter,
        use_cuvid: false,
    }
}
