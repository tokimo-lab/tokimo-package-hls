use crate::hw_detect::get_hw_capabilities;
use crate::pipeline::{HwBackend, PipelineParams, build_pipeline, select_backend};
use crate::types::{AudioStreamInfo, TonemapOptions, needs_audio_transcode};
use std::path::PathBuf;
use std::sync::Arc;
use tokimo_package_ffmpeg::AVHWDeviceContext;
use tokimo_package_ffmpeg::best_encoder_for_audio_codec;
use tokimo_package_ffmpeg::encoding::{calculate_audio_output, calculate_output_video_bitrate};
use tokimo_package_ffmpeg::transcode::{
    CancellationToken, DirectInput, HlsOptions, HlsSegmentType, PauseToken, TranscodeOptions,
};

/// HLS segment duration in seconds.
pub const SEGMENT_DURATION: u32 = 6;

/// Build `TranscodeOptions` for an HLS session.
///
/// Same hardware acceleration strategy as the old CLI arg builder:
/// 1. Full CUDA (CUVID + `scale_cuda` + `tonemap_cuda` + NVENC)
/// 2. NVENC + software filters
/// 3. Full software (libx264)
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn build_transcode_options(
    local_path: Option<&str>,
    audio_stream_index: u32,
    audio_streams: &[AudioStreamInfo],
    output_dir: &str,
    segment_duration: u32,
    seek_seconds: Option<f64>,
    start_segment: u32,
    transcode_video: bool,
    transcode_audio_override: Option<bool>,
    target_audio_codec: Option<&str>,
    tonemap: Option<&TonemapOptions>,
    video_codec: Option<&str>,
    _video_width: Option<u32>,
    _video_height: Option<u32>,
    video_fps: Option<f64>,
    video_bitrate: Option<u64>,
    deinterlace: bool,
    client_supports_hevc: bool,
    cancel: Option<CancellationToken>,
    pause: Option<PauseToken>,
    cached_device_ctx: Option<AVHWDeviceContext>,
    direct_input: Option<Arc<DirectInput>>,
    iso_type: Option<&str>,
) -> TranscodeOptions {
    let caps = get_hw_capabilities();

    let audio = audio_streams.get(audio_stream_index as usize);

    let transcode_audio =
        transcode_audio_override.unwrap_or_else(|| audio.is_none_or(|a| needs_audio_transcode(&a.codec)));

    let channels = audio.and_then(|a| a.channels).unwrap_or(2);
    let (audio_bitrate_kbps, out_channels) = calculate_audio_output(channels);

    // Local files: FFmpeg opens the path directly.
    // Remote files: direct_input (AVIO) is used — `input` is unused in that case.
    // For local Blu-ray ISOs, prefix the path with `bluray:` so libbluray opens the disc.
    let effective_input = match (local_path, iso_type) {
        (Some(path), Some("bluray")) => format!("bluray:{path}"),
        (Some(path), _) => path.to_string(),
        (None, _) => String::new(),
    };
    let playlist_path = format!("{output_dir}/playlist.m3u8");

    // Use mpegts segments for copy mode to avoid hls.js passthrough-remuxer
    // timestamp drift issues. hls.js's mp4-remuxer (used for mpegts→fmp4
    // transmux) keeps initPTS constant after seeking, preventing the uniform
    // timestamp shift that causes video to appear behind subtitles.
    let segment_type = if transcode_video {
        HlsSegmentType::Fmp4
    } else {
        HlsSegmentType::Mpegts
    };
    let seg_ext = segment_type.extension();
    let segment_pattern = format!("{output_dir}/%05d.{seg_ext}");

    // ── Hardware decisions ─────────────────────────────────────────────────
    // Select the best available backend (CUDA > VideoToolbox > VAAPI > QSV > AMF > RKMPP > SW)
    // then build the full pipeline config for that backend.
    //
    // HEVC is preferred whenever the client supports it — mirroring Jellyfin's approach
    // where the client declares codec priority in TranscodingProfiles. Falls back to H.264
    // automatically when the backend has no HEVC encoder or the client cannot decode HEVC.
    let prefer_hevc = client_supports_hevc;

    let out_codec_for_bitrate = if prefer_hevc { "hevc" } else { "h264" };
    let out_bitrate_kbps = if transcode_video {
        calculate_output_video_bitrate(video_bitrate, video_codec.unwrap_or("hevc"), out_codec_for_bitrate)
    } else {
        0
    };

    let hw_backend = if transcode_video {
        select_backend(caps)
    } else {
        HwBackend::Software
    };

    let pipe = if transcode_video {
        Some(build_pipeline(&PipelineParams {
            caps,
            backend: hw_backend,
            video_codec,
            video_fps,
            video_bitrate,
            segment_duration,
            deinterlace,
            tonemap,
            out_bitrate_kbps,
            use_hevc: prefer_hevc,
        }))
    } else {
        None
    };

    // Pre-compute encoder name for logging.
    let selected_encoder = if transcode_video {
        pipe.as_ref().map_or("copy", |p| p.encoder.as_str()).to_string()
    } else {
        "copy".to_string()
    };

    // ── Video codec & encoder options ─────────────────────────────────────
    let (
        vid_codec,
        vid_preset,
        vid_crf,
        vid_bitrate,
        vid_maxrate,
        vid_bufsize,
        vid_profile,
        vid_gop,
        vid_keyint_min,
        force_kf_interval,
    ) = if let Some(ref p) = pipe {
        (
            p.encoder.clone(),
            p.preset.clone(),
            p.crf,
            p.bitrate.clone(),
            p.maxrate.clone(),
            p.bufsize.clone(),
            p.profile.clone(),
            p.gop,
            p.keyint_min,
            p.force_kf,
        )
    } else {
        (
            "copy".to_string(),
            String::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    };

    // ── Video filter chain ─────────────────────────────────────────────────
    let video_filter = pipe.as_ref().and_then(|p| p.video_filter.clone());

    // ── Decode / filter backend ────────────────────────────────────────────
    let decode = pipe.as_ref().and_then(|p| p.decode.clone());
    let filter_backend = pipe.as_ref().and_then(|p| p.filter_backend.clone());
    let use_cuvid = pipe.as_ref().is_some_and(|p| p.use_cuvid);

    // ── Audio codec ────────────────────────────────────────────────────────
    // Use the client-preferred codec (filtered by HLS compatibility, passed from
    // the playback handler). Falls back to AAC when no preference or encoder missing.
    let audio_codec = if transcode_audio {
        target_audio_codec
            .and_then(best_encoder_for_audio_codec)
            .unwrap_or_else(|| best_encoder_for_audio_codec("aac").unwrap_or("aac"))
    } else {
        "copy"
    };

    // ── Pipeline decision log ──────────────────────────────────────────────
    let tonemap_path = if let Some(tm) = tonemap {
        let _ = tm;
        if hw_backend == HwBackend::Cuda && use_cuvid && caps.has_cuda_full {
            "cuda"
        } else if hw_backend == HwBackend::Vaapi && caps.has_vaapi_full {
            "vaapi"
        } else if hw_backend == HwBackend::Qsv && caps.has_qsv_full {
            "qsv"
        } else if hw_backend == HwBackend::VideoToolbox && caps.has_videotoolbox_tonemap {
            "videotoolbox"
        } else if caps.has_tonemapx {
            "tonemapx"
        } else if caps.has_zscale {
            "zscale"
        } else {
            "sw"
        }
    } else {
        ""
    };
    let tonemap_peak = tonemap.map_or(0.0, |t| t.peak);
    let tonemap_desat = tonemap.map_or(0.0, |t| t.desat);
    let tonemap_mode = tonemap.map_or("", |t| t.mode.as_str());

    // pipeline path: dec → filter → enc [backend]
    let pipeline_path = if transcode_video {
        let dec = if use_cuvid {
            "Cuda"
        } else {
            decode.as_deref().unwrap_or("SW")
        };
        let flt = filter_backend.as_deref().unwrap_or("SW");
        format!("{dec} → {flt} → {} [{}]", vid_codec, hw_backend.label())
    } else {
        String::new()
    };

    let vf_str = video_filter.as_deref().unwrap_or("");

    tracing::info!(
        target: "playback::pipeline",
        video_encoder = %selected_encoder,
        audio_encoder = %audio_codec,
        hw_backend = %hw_backend.label(),
        use_cuvid = use_cuvid,
        has_nvenc = caps.has_nvenc,
        has_cuvid = caps.has_cuvid,
        has_cuda = caps.has_cuda_full,
        has_vaapi = caps.has_vaapi,
        has_vaapi_full = caps.has_vaapi_full,
        has_qsv = caps.has_qsv,
        has_qsv_full = caps.has_qsv_full,
        has_videotoolbox = caps.has_videotoolbox,
        has_amf = caps.has_amf,
        has_rkmpp = caps.has_rkmpp,
        has_tonemap = caps.has_tonemap,
        has_tonemapx = caps.has_tonemapx,
        has_zscale = caps.has_zscale,
        tonemap_algo = %tonemap.map_or("", |t| t.algorithm.as_str()),
        tonemap_path = %tonemap_path,
        tonemap_peak = %tonemap_peak,
        tonemap_desat = %tonemap_desat,
        tonemap_mode = %tonemap_mode,
        deinterlace = deinterlace,
        pipeline_path = %pipeline_path,
        video_filter = %vf_str,
        ""
    );

    // ── Seek / accurate-seek ──────────────────────────────────────────────
    let accurate_seek = match seek_seconds {
        Some(_) => transcode_video && transcode_audio,
        None => true,
    };

    // ── HLS options ────────────────────────────────────────────────────────
    // Jellyfin: always VOD for on-demand content. EVENT is for live streams only.
    let hls = HlsOptions {
        segment_duration,
        segment_pattern,
        init_filename: "init.mp4".to_string(),
        playlist_type: "vod".to_string(),
        start_number: start_segment,
        segment_type,
    };

    TranscodeOptions {
        input: PathBuf::from(effective_input),
        output: PathBuf::from(playlist_path),
        video_codec: vid_codec,
        audio_codec: audio_codec.to_string(),
        decode,
        filter_backend,
        preset: vid_preset,
        crf: vid_crf,
        bitrate: vid_bitrate,
        resolution: None,
        duration: None,
        progress: false,
        seek: seek_seconds,
        video_filter,
        video_profile: vid_profile,
        maxrate: vid_maxrate,
        bufsize: vid_bufsize,
        gop: vid_gop,
        keyint_min: vid_keyint_min,
        audio_bitrate: if transcode_audio {
            Some(format!("{audio_bitrate_kbps}k"))
        } else {
            None
        },
        audio_channels: if transcode_audio {
            Some(out_channels as i32)
        } else {
            None
        },
        audio_sample_rate: None,
        cancel,
        pause,
        hls: Some(hls),
        force_key_frames_interval: force_kf_interval,
        accurate_seek,
        cached_device_ctx,
        direct_input,
    }
}

/// Generate a VOD m3u8 playlist.
///
/// Matches Jellyfin `DynamicHlsPlaylistGenerator.CreateMainPlaylist()`:
/// - `#EXT-X-PLAYLIST-TYPE:VOD` with `#EXT-X-ENDLIST`
/// - When `keyframes` is `Some`: cut segments at actual keyframe boundaries
///   (like Jellyfin's `ComputeSegments`).  Each segment aligns to a source
///   keyframe, so seeks can land precisely without a large backward bias.
/// - When `keyframes` is `None`: equal-length segments (fallback for
///   unindexed files such as MPEG-TS Blu-ray remuxes).
///
/// Returns `(playlist_string, segment_start_times_secs)`.
/// `segment_start_times_secs[i]` is the exact start of segment `i` in seconds;
/// used in `seek_restart` to seek to the right keyframe.
pub fn generate_vod_playlist(
    duration_secs: f64,
    segment_duration: u32,
    segment_type: HlsSegmentType,
    keyframes: Option<&[f64]>,
) -> (String, Vec<f64>) {
    let seg_dur = f64::from(segment_duration);
    let seg_ext = segment_type.extension();

    // ── Compute per-segment (start, duration) pairs ──────────────────────────
    let segments: Vec<(f64, f64)> = if let Some(kfs) = keyframes {
        compute_keyframe_segments(kfs, seg_dur, duration_secs)
    } else {
        let count = (duration_secs / seg_dur).ceil() as usize;
        (0..count)
            .map(|i| {
                let start = i as f64 * seg_dur;
                let dur = (duration_secs - start).min(seg_dur);
                (start, dur)
            })
            .collect()
    };

    let start_times: Vec<f64> = segments.iter().map(|&(s, _)| s).collect();

    // ── #EXT-X-TARGETDURATION must be >= ceil(max segment duration) ──────────
    let max_dur = segments.iter().map(|&(_, d)| d).fold(seg_dur, f64::max).ceil() as u32;

    // ── Build playlist ────────────────────────────────────────────────────────
    let mut lines = Vec::with_capacity(segments.len() * 2 + 10);
    lines.push("#EXTM3U".to_string());

    match segment_type {
        HlsSegmentType::Fmp4 => lines.push("#EXT-X-VERSION:7".to_string()),
        HlsSegmentType::Mpegts => lines.push("#EXT-X-VERSION:3".to_string()),
    }

    lines.push(format!("#EXT-X-TARGETDURATION:{max_dur}"));
    lines.push("#EXT-X-MEDIA-SEQUENCE:0".to_string());
    lines.push("#EXT-X-PLAYLIST-TYPE:VOD".to_string());

    if segment_type == HlsSegmentType::Fmp4 {
        lines.push("#EXT-X-MAP:URI=\"init.mp4\"".to_string());
    }

    for (i, (_, dur)) in segments.iter().enumerate() {
        lines.push(format!("#EXTINF:{dur:.6},"));
        lines.push(format!("{i:05}.{seg_ext}"));
    }

    lines.push("#EXT-X-ENDLIST".to_string());
    lines.push(String::new());

    (lines.join("\n"), start_times)
}

/// Compute keyframe-aligned segment boundaries.
///
/// Mirrors Jellyfin `DynamicHlsPlaylistGenerator.ComputeSegments()`:
/// for each ideal boundary at N × desired_secs, find the keyframe
/// nearest to that boundary (but strictly after the previous cut).
///
/// Returns `Vec<(start_secs, duration_secs)>` for each segment.
fn compute_keyframe_segments(keyframes: &[f64], desired_secs: f64, duration_secs: f64) -> Vec<(f64, f64)> {
    let mut result = Vec::new();
    let mut last_cut = 0.0f64;
    let mut segment_num = 1u64;

    loop {
        let target = segment_num as f64 * desired_secs;
        if target >= duration_secs {
            break;
        }

        // Find the keyframe nearest to this ideal boundary, but only among
        // keyframes strictly after the previous cut.
        let nearest = keyframes
            .iter()
            .filter(|&&kf| kf > last_cut && kf < duration_secs)
            .min_by(|&&a, &&b| {
                (a - target)
                    .abs()
                    .partial_cmp(&(b - target).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied();

        if let Some(kf) = nearest {
            result.push((last_cut, kf - last_cut));
            last_cut = kf;
        }

        segment_num += 1;
    }

    // Final segment up to duration.
    if duration_secs > last_cut {
        result.push((last_cut, duration_secs - last_cut));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vod_playlist_fmp4() {
        let (pl, starts) = generate_vod_playlist(15.0, 6, HlsSegmentType::Fmp4, None);
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(pl.contains("#EXT-X-VERSION:7"));
        assert!(pl.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(pl.contains("00000.tokimo"));
        assert!(pl.contains("00001.tokimo"));
        assert!(pl.contains("00002.tokimo"));
        assert!(pl.contains("#EXT-X-ENDLIST"));
        assert_eq!(starts, vec![0.0, 6.0, 12.0]);
    }

    #[test]
    fn test_vod_playlist_mpegts() {
        let (pl, starts) = generate_vod_playlist(15.0, 6, HlsSegmentType::Mpegts, None);
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(pl.contains("#EXT-X-VERSION:3"));
        assert!(!pl.contains("#EXT-X-MAP"));
        assert!(pl.contains("00000.ts"));
        assert!(pl.contains("00001.ts"));
        assert!(pl.contains("00002.ts"));
        assert!(pl.contains("#EXT-X-ENDLIST"));
        assert_eq!(starts, vec![0.0, 6.0, 12.0]);
    }

    #[test]
    fn test_vod_playlist_keyframe_based() {
        // Keyframes at 6.2, 11.9, 18.1 → cuts at first kf >= 6s, >= 12s, >= 18s
        let kfs = vec![1.0, 3.5, 6.2, 9.0, 11.9, 14.2, 18.1, 21.0];
        let (pl, starts) = generate_vod_playlist(24.0, 6, HlsSegmentType::Mpegts, Some(&kfs));
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        // Segments: [0, 6.2), [6.2, 11.9), [11.9, 18.1), [18.1, 24)
        assert_eq!(starts, vec![0.0, 6.2, 11.9, 18.1]);
        // All four segments present
        assert!(pl.contains("00000.ts"));
        assert!(pl.contains("00001.ts"));
        assert!(pl.contains("00002.ts"));
        assert!(pl.contains("00003.ts"));
        assert!(!pl.contains("00004.ts"));
        assert!(pl.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_needs_transcode() {
        // Canonical names — verifies the re-export from tokimo_package_ffmpeg::encoding works
        assert!(needs_audio_transcode("ac3"));
        assert!(needs_audio_transcode("truehd"));
        assert!(needs_audio_transcode("DTS"));
        assert!(needs_audio_transcode("pcm_s16le"));
        assert!(!needs_audio_transcode("aac"));
        assert!(!needs_audio_transcode("mp3"));
        assert!(!needs_audio_transcode("opus"));
    }
}
