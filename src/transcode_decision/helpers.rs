use std::collections::HashSet;
use std::sync::LazyLock;

// ── Audio codecs browsers can typically decode — fallback when client doesn't report ──
pub(super) static BROWSER_SUPPORTED_AUDIO_FALLBACK: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["aac", "mp3", "opus", "flac", "vorbis", "alac"]));

// ── Container formats browsers can play directly ─────────────────────────────
pub(super) static BROWSER_DIRECT_PLAY_CONTAINERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "mp4", "m4v", "webm", "mov", // Audio containers
        "m4a", "mp3", "ogg", "opus", "wav", "flac", "aac",
    ])
});

// ── Network filesystem source types ──────────────────────────────────────────
pub(super) static NET_FS_SOURCE_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "smb",
        "nfs",
        "webdav",
        "ftp",
        "sftp",
        "s3",
        "115cloud",
        "aliyundrive",
        "baidu_netdisk",
        "quark",
    ])
});

// ── Containers whose H.264 streams typically use open-GOP ──────────────────
pub(super) static OPEN_GOP_CONTAINERS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["m2ts", "mts"]));

// ── H.264 / HEVC profile score tables — 1:1 from EncodingHelper.cs ───────────
const H264_PROFILES: &[&str] = &[
    "constrainedbaseline",
    "baseline",
    "extended",
    "main",
    "high",
    "progressivehigh",
    "constrainedhigh",
    "high10",
];

const HEVC_PROFILES: &[&str] = &["main", "main10"];

pub(super) fn video_profile_score(codec: &str, profile: &str) -> i32 {
    let p: String = profile.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let list = match codec {
        "h264" => H264_PROFILES,
        "hevc" => HEVC_PROFILES,
        _ => return -1,
    };
    list.iter().position(|&x| x == p).map_or(-1, |i| i as i32)
}

/// Returns true if the file is audio-only (no video stream).
pub fn is_audio_only_file(video_codec: Option<&str>, mime_type: Option<&str>) -> bool {
    if video_codec.is_none() {
        return true;
    }
    if let Some(mime) = mime_type
        && mime.starts_with("audio/")
    {
        return true;
    }
    false
}

/// Returns true if the source type is a network filesystem.
pub fn is_net_fs_source(source_type: &str) -> bool {
    NET_FS_SOURCE_TYPES.contains(source_type)
}
