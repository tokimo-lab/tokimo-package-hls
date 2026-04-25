use serde::{Deserialize, Serialize};

// Re-export from the shared encoding module — single source of truth.
pub use tokimo_package_ffmpeg::encoding::needs_audio_transcode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStreamInfo {
    pub index: u32,
    pub codec: String,
    pub channels: Option<u32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub is_default: Option<bool>,
}

/// HDR→SDR tone mapping options.
/// Mirrors Jellyfin's `EncodingOptions` tone mapping fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TonemapOptions {
    /// Tone mapping algorithm: bt2390 (Jellyfin default), hable, reinhard, mobius, clip, linear, gamma, none
    #[serde(default = "default_tonemap_algorithm")]
    pub algorithm: String,
    /// Peak brightness in nits. Jellyfin default: 100
    #[serde(default = "default_tonemap_peak")]
    pub peak: f64,
    /// Desaturation (0 = off). Jellyfin default: 0
    #[serde(default)]
    pub desat: f64,
    /// Tone mapping mode: auto (default, not appended), max, rgb, lum, itp.
    /// Jellyfin: "auto" = omit param, "max"/"rgb" require `FFmpeg` ≥5.1.3,
    /// "lum"/"itp" require `FFmpeg` ≥7.0.1.
    #[serde(default = "default_tonemap_mode")]
    pub mode: String,
    /// Algorithm-specific parameter (e.g. knee point for reinhard).
    /// Jellyfin default: 0 (not appended when 0).
    #[serde(default)]
    pub param: f64,
    /// Output range: auto (default, not appended), tv, pc.
    /// Jellyfin default: "auto".
    #[serde(default = "default_tonemap_range")]
    pub range: String,
}

fn default_tonemap_algorithm() -> String {
    "bt2390".to_string()
}
fn default_tonemap_peak() -> f64 {
    100.0
}
fn default_tonemap_mode() -> String {
    "auto".to_string()
}
fn default_tonemap_range() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub file_id: String,
    /// Direct filesystem path for local files. When set, `FFmpeg` reads from disk
    /// directly (no AVIO overhead). Remote sources use `direct_input` (AVIO).
    #[serde(default)]
    pub local_path: Option<String>,
    /// Total duration of the media file in seconds.
    pub duration_secs: f64,
    /// Which audio stream index to include (0-based among audio streams).
    pub audio_stream_index: u32,
    /// All audio streams in the file — used to decide copy vs transcode.
    pub audio_streams: Vec<AudioStreamInfo>,
    /// Whether video needs re-encoding (not just stream copy).
    #[serde(default)]
    pub transcode_video: bool,
    /// Whether audio needs re-encoding. When `Some(true)`, forces transcode;
    /// when `Some(false)`, forces stream copy. When `None`, falls back to the
    /// built-in `needs_audio_transcode()` heuristic.
    #[serde(default)]
    pub transcode_audio: Option<bool>,
    /// Target audio codec for transcoding (e.g. "aac", "ac3", "opus").
    /// Selected by the server based on the client's ordered `audioCodecs` preference list,
    /// filtered by HLS container compatibility. Defaults to "aac" when absent.
    #[serde(default)]
    pub target_audio_codec: Option<String>,
    /// HDR→SDR tone mapping options. Only used when `transcode_video` is true.
    #[serde(default)]
    pub tonemap: Option<TonemapOptions>,
    /// Source video codec (e.g. "hevc", "h264") — used to select CUVID decoder.
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Source video width in pixels — used with `video_height` to decide HEVC output for 4K.
    #[serde(default)]
    pub video_width: Option<u32>,
    /// Source video height in pixels — used with `video_width` to decide HEVC output for 4K.
    #[serde(default)]
    pub video_height: Option<u32>,
    /// Source video framerate — used for NVENC GOP calculation.
    /// Jellyfin: `ceil(segmentLength × fps)` for `-g:v:0`.
    #[serde(default)]
    pub video_fps: Option<f64>,
    /// Source video bitrate in bits/sec — used for dynamic output bitrate calculation.
    /// Jellyfin: ScaleBitrate(min(source, cap), inputCodec, outputCodec).
    #[serde(default)]
    pub video_bitrate: Option<u64>,
    /// Whether the source video is interlaced and needs deinterlacing.
    #[serde(default)]
    pub deinterlace: bool,
    // ── Playback tracking (optional — for server-side progress persistence) ──
    /// Authenticated user ID (UUID). When set, Rust writes playback progress
    /// directly to `user_media_states` / `watch_histories`.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Movie ID (UUID) the file belongs to.
    #[serde(default)]
    pub video_item_id: Option<String>,
    /// Episode ID (UUID) the file belongs to.
    #[serde(default)]
    pub episode_id: Option<String>,
    /// ISO disc type — `"bluray"` or `"dvd"`. Only set for `.iso` files.
    /// When set, the HLS pipeline uses the appropriate `FFmpeg` input strategy:
    /// - local Blu-ray ISO: `bluray:"/path/file.iso"` (libbluray)
    /// - remote Blu-ray ISO: AVIO-based M2TS extraction from within the ISO (UDF reader)
    #[serde(default)]
    pub iso_type: Option<String>,
    /// Whether the client can decode HEVC. When `false`, the transcoder will
    /// never output HEVC even for HDR/4K content (falls back to H.264).
    /// Derived from the client's reported `video_codecs` capability list.
    #[serde(default = "default_true")]
    pub client_supports_hevc: bool,
    /// Direct VFS input for custom AVIO — bypasses HTTP for `FFmpeg` reads.
    /// Set by the server handler when a remote VFS is available.
    #[serde(skip)]
    pub direct_input: Option<std::sync::Arc<tokimo_package_ffmpeg::DirectInput>>,
}

fn default_true() -> bool {
    true
}

/// Snapshot of an active HLS session's playback state.
#[derive(Debug, Clone)]
pub struct PlaybackSnapshot {
    pub session_id: String,
    pub file_id: String,
    pub user_id: Option<String>,
    pub video_item_id: Option<String>,
    pub episode_id: Option<String>,
    pub duration_secs: f64,
    /// Estimated playback position in seconds (`segment_index` × `SEGMENT_DURATION`).
    pub position_secs: f64,
    /// Label for `watch_histories.client_name` (e.g. "HLS Server", "Direct Play").
    pub client_name: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HlsSessionInfo {
    pub session_id: String,
    pub playlist_url: String,
}

/// Per-session runtime state exposed for status queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Starting,
    Running,
    Finished,
    Failed,
    Stopped,
}
