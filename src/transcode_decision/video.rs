use super::helpers::{OPEN_GOP_CONTAINERS, video_profile_score};
use super::types::{ClientProfile, VideoStreamInfo};
use tokimo_package_ffmpeg::normalize_video_codec;

/// Returns `true` if transcoding is needed (cannot direct play).
pub fn needs_video_transcode(
    video_codec: Option<&str>,
    video_profile: Option<&str>,
    hdr_type: Option<&str>,
    vs: &VideoStreamInfo,
    profile: &ClientProfile,
) -> bool {
    video_transcode_reason(video_codec, video_profile, hdr_type, vs, profile).is_some()
}

/// Returns the specific reason the video stream needs transcoding, or None if
/// it can be stream-copied. Mirrors Jellyfin's `TranscodeReason` flags.
#[allow(clippy::too_many_lines)]
pub fn video_transcode_reason(
    video_codec: Option<&str>,
    video_profile: Option<&str>,
    hdr_type: Option<&str>,
    vs: &VideoStreamInfo,
    profile: &ClientProfile,
) -> Option<String> {
    let raw_codec = video_codec?;
    let source_codec = normalize_video_codec(raw_codec);

    let supported_norm: Vec<String> = if profile.supported_vc.is_empty() {
        vec!["h264".to_string()]
    } else {
        profile.supported_vc.iter().map(|c| normalize_video_codec(c)).collect()
    };

    // 1. Codec match
    if !supported_norm.iter().any(|c| c == &source_codec) {
        return Some(format!(
            "VideoCodecNotSupported (source={source_codec}, client supports=[{}])",
            supported_norm.join(",")
        ));
    }

    // 2. Interlaced
    if vs.is_interlaced == Some(true) {
        return Some("InterlacedVideoNotSupported".to_string());
    }

    // 3. Anamorphic
    if !profile.supports_anamorphic && vs.is_anamorphic == Some(true) {
        return Some("AnamorphicVideoNotSupported".to_string());
    }

    // 4. Profile
    if let Some(vp) = video_profile {
        let src_normalized = vp.to_lowercase().replace(' ', "");
        let src_score = video_profile_score(&source_codec, vp);

        if source_codec == "h264" {
            if profile.h264_profiles.is_empty() {
                let browser_max = video_profile_score("h264", "high");
                if src_score != -1 && src_score > browser_max {
                    return Some(format!("VideoProfileNotSupported (h264 profile={vp})"));
                }
            } else {
                let max_client_score = profile
                    .h264_profiles
                    .iter()
                    .map(|p| video_profile_score("h264", p))
                    .max()
                    .unwrap_or(-1);
                if src_score != -1 && max_client_score != -1 && src_score > max_client_score {
                    return Some(format!(
                        "VideoProfileNotSupported (h264 profile={vp}, client max=[{}])",
                        profile.h264_profiles.join(",")
                    ));
                }
            }
        }
        if source_codec == "hevc" {
            if profile.hevc_profiles.is_empty() {
                let browser_max = video_profile_score("hevc", "main10");
                if src_score != -1 && src_score > browser_max {
                    return Some(format!("VideoProfileNotSupported (hevc profile={vp})"));
                }
            } else {
                let max_client_score = profile
                    .hevc_profiles
                    .iter()
                    .map(|p| video_profile_score("hevc", p))
                    .max()
                    .unwrap_or(-1);
                if src_score != -1 && max_client_score != -1 && src_score > max_client_score {
                    return Some(format!(
                        "VideoProfileNotSupported (hevc profile={vp}, client max=[{}])",
                        profile.hevc_profiles.join(",")
                    ));
                }
            }
        }
        if source_codec == "av1" && src_normalized != "main" && !src_normalized.is_empty() {
            return Some(format!(
                "VideoProfileNotSupported (av1 profile={vp}, only 'main' supported)"
            ));
        }
    }

    // 5. HDR / VideoRangeType
    let source_range_type = hdr_type.map(|ht| {
        match ht.to_lowercase().as_str() {
            "sdr" => "SDR",
            "hdr10" => "HDR10",
            "hdr10plus" | "hdr10_plus" => "HDR10Plus",
            "dolby_vision" | "dovi" => "DOVI",
            "dolby_vision_hdr10" => "DOVIWithHDR10",
            "dolby_vision_hdr10_plus" => "DOVIWithHDR10Plus",
            "dolby_vision_hlg" => "DOVIWithHLG",
            "dolby_vision_sdr" => "DOVIWithSDR",
            "dolby_vision_el" => "DOVIWithEL",
            "dolby_vision_el_hdr10_plus" => "DOVIWithELHDR10Plus",
            "dovi_invalid" => "DOVIInvalid",
            "hlg" => "HLG",
            _ => return ht.to_uppercase(),
        }
        .to_string()
    });

    if let Some(ref srt) = source_range_type {
        let sr = &profile.supported_range_types;
        let has = |s: &str| sr.iter().any(|r| r == s);

        let needs_transcode = match srt.as_str() {
            "SDR" => !has("SDR") && !has("DOVIWithSDR"),
            "HDR10" => !has("HDR10") && !has("DOVIWithHDR10"),
            "HDR10Plus" => !has("HDR10Plus") && !has("HDR10") && !has("DOVIWithHDR10Plus") && !has("DOVIWithHDR10"),
            "HLG" => !has("HLG") && !has("DOVIWithHLG"),
            "DOVI" => {
                !has("DOVI")
                    && !has("DOVIWithHDR10")
                    && !has("DOVIWithHLG")
                    && !has("DOVIWithSDR")
                    && !has("DOVIWithEL")
                    && !has("DOVIInvalid")
            }
            "DOVIWithHDR10" => !has("DOVIWithHDR10") && !has("DOVI") && !has("HDR10"),
            "DOVIWithHDR10Plus" => {
                !has("DOVIWithHDR10Plus") && !has("DOVIWithHDR10") && !has("DOVI") && !has("HDR10Plus") && !has("HDR10")
            }
            "DOVIWithHLG" => !has("DOVIWithHLG") && !has("DOVI") && !has("HLG"),
            "DOVIWithSDR" => !has("DOVIWithSDR") && !has("DOVI") && !has("SDR"),
            "DOVIWithEL" => !has("DOVIWithEL") && !has("DOVI"),
            "DOVIWithELHDR10Plus" => {
                !has("DOVIWithELHDR10Plus") && !has("DOVIWithEL") && !has("DOVI") && !has("HDR10Plus") && !has("HDR10")
            }
            "DOVIInvalid" => !has("DOVIInvalid") && !has("DOVI") && !has("HDR10"),
            other => !has(other),
        };
        if needs_transcode {
            return Some(format!(
                "VideoRangeTypeNotSupported (source={srt}, client supports=[{}])",
                sr.join(",")
            ));
        }
    }

    // 6. Bit depth
    if let Some(bd) = vs.bit_depth {
        if bd > 8 && source_codec == "h264" {
            let supports_high10 = profile.h264_profiles.iter().any(|p| p == "high10");
            if !supports_high10 {
                return Some(format!(
                    "VideoBitDepthNotSupported (h264 {bd}-bit, client lacks high 10)"
                ));
            }
        }
        if let Some(max_bd) = profile.max_video_bit_depth
            && bd > max_bd
        {
            return Some(format!(
                "VideoBitDepthNotSupported ({source_codec} {bd}-bit > max={max_bd})"
            ));
        }
    }

    // 7. Level
    if let Some(level) = vs.level {
        if source_codec == "h264" {
            let level_norm = if level > 10.0 { level / 10.0 } else { level };
            let max_level = profile
                .max_h264_level
                .map_or(5.2, |l| if l > 10.0 { l / 10.0 } else { l });
            if level_norm > max_level {
                return Some(format!(
                    "VideoLevelNotSupported (h264 level={level_norm} > max={max_level})"
                ));
            }
        }
        if source_codec == "hevc" {
            let max_level = profile.max_hevc_level.unwrap_or(120);
            let src_level = if level <= 10.0 {
                (level * 30.0) as i32
            } else {
                level as i32
            };
            if src_level > max_level {
                return Some(format!(
                    "VideoLevelNotSupported (hevc level={src_level} > max={max_level})"
                ));
            }
        }
        if source_codec == "av1"
            && let Some(max_level) = profile.max_av1_level
        {
            let src_level = level as i32;
            if src_level > max_level {
                return Some(format!(
                    "VideoLevelNotSupported (av1 level={src_level} > max={max_level})"
                ));
            }
        }
    }

    // 8. Bitrate limit
    if let Some(max_br) = profile.max_bitrate {
        let src_bitrate_bps = vs.bitrate_kbps.map_or(40_000_000, |k| k * 1000);
        if src_bitrate_bps > max_br {
            return Some(format!(
                "VideoBitrateNotSupported ({src_bitrate_bps} bps > max={max_br} bps)"
            ));
        }
    }

    // 9. Resolution limit
    if let Some(mw) = profile.max_width
        && let Some(vw) = vs.width
        && vw > mw
    {
        return Some(format!("VideoResolutionNotSupported (width={vw} > max={mw})"));
    }
    if let Some(mh) = profile.max_height
        && let Some(vh) = vs.height
        && vh > mh
    {
        return Some(format!("VideoResolutionNotSupported (height={vh} > max={mh})"));
    }

    // 10. RefFrames limit
    if let Some(mr) = profile.max_ref_frames
        && let Some(vr) = vs.ref_frames
        && vr > mr
    {
        return Some(format!("RefFramesNotSupported ({vr} > max={mr})"));
    }

    // 11. Framerate limit
    if let Some(mf) = profile.max_framerate
        && let Some(vf) = vs.frame_rate
        && vf > mf + 0.05
    {
        return Some(format!("VideoFramerateNotSupported ({vf:.2} fps > max={mf} fps)"));
    }

    // 12. HEVC-specific framerate limit (Safari: 60fps max)
    if source_codec == "hevc"
        && let Some(mf) = profile.hevc_max_framerate
        && let Some(vf) = vs.frame_rate
        && vf > mf + 0.05
    {
        return Some(format!("VideoFramerateNotSupported (HEVC {vf:.2} fps > max={mf} fps)"));
    }

    None
}

/// Check if the source HDR type represents actual HDR content.
pub fn is_hdr(hdr_type: Option<&str>) -> bool {
    match hdr_type {
        None => false,
        Some(ht) => {
            let lower = ht.to_lowercase();
            !lower.is_empty() && lower != "sdr"
        }
    }
}

/// Returns a reason if the video codec tag is incompatible with the client.
pub fn codec_tag_transcode_reason(
    video_codec: Option<&str>,
    vs: &VideoStreamInfo,
    profile: &ClientProfile,
    path: &str,
) -> Option<String> {
    let raw_codec = video_codec?;
    let source_codec = normalize_video_codec(raw_codec);

    // 1. HEVC codec tags (Safari requires hvc1/dvh1)
    if source_codec == "hevc" && !profile.hevc_codec_tags.is_empty() {
        match &vs.codec_tag {
            Some(tag) => {
                let tag_lower = tag.to_lowercase();
                if !profile.hevc_codec_tags.iter().any(|t| t == &tag_lower) {
                    return Some(format!(
                        "VideoCodecTagNotSupported (tag={tag}, client requires=[{}])",
                        profile.hevc_codec_tags.join(",")
                    ));
                }
            }
            None => {
                return Some("VideoCodecTagNotSupported (tag unknown, client has requirements)".to_string());
            }
        }
    }

    // 2. AVI container + H.264 non-AVC
    if source_codec == "h264" && vs.is_avc == Some(false) {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        if ext == "avi" {
            return Some("VideoCodecTagNotSupported (h264 non-AVC in AVI)".to_string());
        }
    }

    None
}

/// Returns a reason string if the container is known to use open-GOP H.264
/// encoding that breaks MSE seek.
pub fn open_gop_transcode_reason(path: &str, video_codec: Option<&str>) -> Option<String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let codec = video_codec.map(str::to_lowercase).unwrap_or_default();
    if OPEN_GOP_CONTAINERS.contains(ext.as_str()) && codec == "h264" {
        Some("OpenGOP (m2ts H.264 non-IDR I-slices, MSE seek incompatible)".to_string())
    } else {
        None
    }
}
