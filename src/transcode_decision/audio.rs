use super::helpers::BROWSER_SUPPORTED_AUDIO_FALLBACK;
use super::types::{AudioInfo, ClientProfile};
use tokimo_package_ffmpeg::normalize_audio_codec;

/// Returns true if the audio codec needs transcoding (browser cannot decode it).
pub fn needs_audio_transcode(audio_codec: Option<&str>) -> bool {
    audio_transcode_reason(audio_codec, None, &ClientProfile::default(), &[]).is_some()
}

/// Returns the specific reason the audio stream needs transcoding, or None if
/// it can be stream-copied. Mirrors Jellyfin's `DirectPlayProfile` + `CanStreamCopyAudio`.
pub fn audio_transcode_reason(
    audio_codec: Option<&str>,
    audio_info: Option<&AudioInfo>,
    profile: &ClientProfile,
    client_audio_codecs: &[String],
) -> Option<String> {
    let codec = audio_codec?;
    let normalized = normalize_audio_codec(codec);

    // 1. Codec match — whitelist approach (Jellyfin: DirectPlayProfile.SupportsAudioCodec)
    let codec_supported = if client_audio_codecs.is_empty() {
        BROWSER_SUPPORTED_AUDIO_FALLBACK.contains(normalized.as_str())
    } else {
        client_audio_codecs.iter().any(|c| c == &normalized)
    };
    if !codec_supported {
        return Some(format!("AudioCodecNotSupported ({normalized})"));
    }

    let info = audio_info?;

    // 2. Audio bit depth
    if let Some(max_bd) = profile.max_audio_bit_depth
        && let Some(bd) = info.bit_depth
        && bd > max_bd
    {
        return Some(format!("AudioBitDepthNotSupported ({bd}-bit > max={max_bd})"));
    }

    // 3. Audio channels
    if let Some(max_ch) = profile.max_audio_channels
        && let Some(ch) = info.channels
    {
        if ch <= 0 {
            return Some("AudioChannelsNotSupported (invalid: ≤0)".to_string());
        }
        if ch > i64::from(max_ch) {
            return Some(format!("AudioChannelsNotSupported ({ch}ch > max={max_ch})"));
        }
    }

    // 4. Audio sample rate
    if let Some(max_sr) = profile.max_audio_sample_rate
        && let Some(sr) = info.sample_rate
    {
        if sr <= 0 {
            return Some("AudioSampleRateNotSupported (invalid: ≤0)".to_string());
        }
        if sr > i64::from(max_sr) {
            return Some(format!("AudioSampleRateNotSupported ({sr} > max={max_sr})"));
        }
    }

    // 5. Audio bitrate
    if let Some(max_br) = profile.max_audio_bitrate
        && let Some(br) = info.bitrate
        && br > max_br
    {
        return Some(format!("AudioBitrateNotSupported ({br} > max={max_br})"));
    }

    // 6. HE-AAC profile (most browsers support it; only block if client explicitly excludes it)
    if let Some(ref ap) = info.profile {
        let ap_lower = ap.to_lowercase();
        if normalized == "aac" && (ap_lower.contains("he-aac") || ap_lower.contains("he_aac") || ap_lower == "lc+sbr") {
            // Not blocked by default — handled via client condition if needed.
        }
    }

    None
}
