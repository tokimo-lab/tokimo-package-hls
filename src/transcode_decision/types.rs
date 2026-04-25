/// Minimal audio stream info needed for transcode decisions.
/// Populated from ffprobe JSON stored in the database.
#[derive(Debug, Clone, Default)]
pub struct AudioInfo {
    pub channels: Option<i64>,
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i32>,
    pub profile: Option<String>,
}

/// Client device profile parsed from query string parameters.
/// Mirrors Jellyfin's `DeviceProfile` / `StreamingRequest`.
#[derive(Debug, Clone)]
pub struct ClientProfile {
    pub supported_vc: Vec<String>,
    pub supported_range_types: Vec<String>,
    pub max_h264_level: Option<f64>,
    pub max_hevc_level: Option<i32>,
    pub max_bitrate: Option<i64>,
    pub max_width: Option<i32>,
    pub max_height: Option<i32>,
    pub max_ref_frames: Option<i32>,
    pub max_framerate: Option<f64>,
    // Jellyfin-parity additions
    pub supports_anamorphic: bool,
    pub hevc_codec_tags: Vec<String>,
    pub max_video_bit_depth: Option<i32>,
    pub max_audio_channels: Option<i32>,
    pub max_audio_bitrate: Option<i64>,
    pub max_audio_sample_rate: Option<i32>,
    pub max_audio_bit_depth: Option<i32>,
    /// Safari-specific: HEVC max framerate (60fps). Jellyfin: hevcCodecProfileConditions.
    pub hevc_max_framerate: Option<f64>,
    /// AV1 max level (15-19). Jellyfin: browserDeviceProfile.js.
    pub max_av1_level: Option<i32>,
    /// Client-reported H.264 profiles ("high|main|baseline|constrained baseline|high 10").
    pub h264_profiles: Vec<String>,
    /// Client-reported HEVC profiles ("main|main 10").
    pub hevc_profiles: Vec<String>,
}

impl ClientProfile {
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        vc: &str,
        vr: &str,
        h264_level: Option<&str>,
        hevc_level: Option<&str>,
        max_bitrate: Option<&str>,
        max_width: Option<&str>,
        max_height: Option<&str>,
        max_ref_frames: Option<&str>,
        max_framerate: Option<&str>,
        supports_anamorphic: Option<&str>,
        hevc_codec_tags: Option<&str>,
        max_video_bit_depth: Option<&str>,
        max_audio_channels: Option<&str>,
        max_audio_bitrate: Option<&str>,
        max_audio_sample_rate: Option<&str>,
        max_audio_bit_depth: Option<&str>,
        hevc_max_framerate: Option<&str>,
        av1_level: Option<&str>,
        h264_profiles: Option<&str>,
        hevc_profiles: Option<&str>,
    ) -> Self {
        let supported_vc: Vec<String> = if vc.is_empty() {
            vec![]
        } else {
            vc.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let supported_range_types: Vec<String> = if vr.is_empty() {
            vec!["SDR".to_string()]
        } else {
            vr.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let parse_int = |v: Option<&str>| v.and_then(|s| s.parse::<i64>().ok());
        let parse_i32 = |v: Option<&str>| v.and_then(|s| s.parse::<i32>().ok());
        let parse_f64 = |v: Option<&str>| v.and_then(|s| s.parse::<f64>().ok());

        let hevc_tags: Vec<String> = hevc_codec_tags
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let parse_profiles = |v: Option<&str>| -> Vec<String> {
            v.map(|s| {
                s.split('|')
                    .map(|p| p.trim().to_lowercase().replace(' ', ""))
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default()
        };

        Self {
            supported_vc,
            supported_range_types,
            max_h264_level: parse_f64(h264_level),
            max_hevc_level: parse_i32(hevc_level),
            max_bitrate: parse_int(max_bitrate),
            max_width: parse_i32(max_width),
            max_height: parse_i32(max_height),
            max_ref_frames: parse_i32(max_ref_frames),
            max_framerate: parse_f64(max_framerate),
            supports_anamorphic: supports_anamorphic != Some("0"),
            hevc_codec_tags: hevc_tags,
            max_video_bit_depth: parse_i32(max_video_bit_depth),
            max_audio_channels: parse_i32(max_audio_channels),
            max_audio_bitrate: parse_int(max_audio_bitrate),
            max_audio_sample_rate: parse_i32(max_audio_sample_rate),
            max_audio_bit_depth: parse_i32(max_audio_bit_depth),
            hevc_max_framerate: parse_f64(hevc_max_framerate),
            max_av1_level: parse_i32(av1_level),
            h264_profiles: parse_profiles(h264_profiles),
            hevc_profiles: parse_profiles(hevc_profiles),
        }
    }
}

impl Default for ClientProfile {
    fn default() -> Self {
        Self {
            supported_vc: vec![],
            supported_range_types: vec!["SDR".to_string()],
            max_h264_level: None,
            max_hevc_level: None,
            max_bitrate: None,
            max_width: None,
            max_height: None,
            max_ref_frames: None,
            max_framerate: None,
            supports_anamorphic: true,
            hevc_codec_tags: vec![],
            max_video_bit_depth: None,
            max_audio_channels: None,
            max_audio_bitrate: None,
            max_audio_sample_rate: None,
            max_audio_bit_depth: None,
            hevc_max_framerate: None,
            max_av1_level: None,
            h264_profiles: vec![],
            hevc_profiles: vec![],
        }
    }
}

/// Video stream metadata extracted from the raw ffprobe JSON.
#[derive(Debug, Default)]
pub struct VideoStreamInfo {
    pub is_interlaced: Option<bool>,
    pub level: Option<f64>,
    pub bit_depth: Option<i32>,
    pub is_avc: Option<bool>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub ref_frames: Option<i32>,
    pub frame_rate: Option<f64>,
    pub bitrate_kbps: Option<i64>,
    pub codec_tag: Option<String>,
    pub is_anamorphic: Option<bool>,
}

impl VideoStreamInfo {
    /// Parse from a `serde_json::Value` (the raw ffprobe `video_streams` JSON column).
    pub fn from_json(val: Option<&serde_json::Value>) -> Self {
        let Some(obj) = val.and_then(|v| v.as_object()) else {
            return Self::default();
        };

        let is_interlaced = obj
            .get("field_order")
            .and_then(|v| v.as_str())
            .map(|fo| !matches!(fo, "progressive" | "unknown" | ""));

        let bit_depth = obj
            .get("bits_per_raw_sample")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i32>().ok())
            .or_else(|| {
                obj.get("pix_fmt").and_then(|v| v.as_str()).and_then(|pf| {
                    if pf.contains("10le") || pf.contains("10be") || pf.contains("p010") {
                        Some(10)
                    } else if pf.contains("12le") || pf.contains("12be") {
                        Some(12)
                    } else {
                        None
                    }
                })
            });

        let frame_rate = obj
            .get("avg_frame_rate")
            .and_then(|v| v.as_str())
            .and_then(Self::parse_frame_rate)
            .or_else(|| {
                obj.get("r_frame_rate")
                    .and_then(|v| v.as_str())
                    .and_then(Self::parse_frame_rate)
            });

        let bitrate_kbps = obj
            .get("bit_rate")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v.as_i64()))
            .map(|b| b / 1000);

        let codec_tag = obj
            .get("codec_tag_string")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        let is_anamorphic = obj
            .get("sample_aspect_ratio")
            .and_then(|v| v.as_str())
            .map(|sar| !matches!(sar, "1:1" | "0:1" | ""));

        Self {
            is_interlaced,
            level: None,
            bit_depth,
            is_avc: None,
            width: obj.get("width").and_then(serde_json::Value::as_i64).map(|v| v as i32),
            height: obj.get("height").and_then(serde_json::Value::as_i64).map(|v| v as i32),
            ref_frames: None,
            frame_rate,
            bitrate_kbps,
            codec_tag,
            is_anamorphic,
        }
    }

    fn parse_frame_rate(s: &str) -> Option<f64> {
        if let Some((num_s, den_s)) = s.split_once('/') {
            let num: f64 = num_s.parse().ok()?;
            let den: f64 = den_s.parse().ok()?;
            if den > 0.0 { Some(num / den) } else { None }
        } else {
            s.parse::<f64>().ok()
        }
    }
}
