/// Transcode decision engine — determines whether a media file can be played
/// directly by the browser or needs HLS transcoding.
///
/// 1:1 port of the Jellyfin-compatible logic from the TypeScript server
/// (packages/server/src/main.ts, lines ~230-667).
mod audio;
mod container;
mod helpers;
mod types;
mod video;

pub use audio::{audio_transcode_reason, needs_audio_transcode};
pub use container::{container_transcode_reason, needs_container_transcode};
pub use helpers::{is_audio_only_file, is_net_fs_source};
pub use types::{AudioInfo, ClientProfile, VideoStreamInfo};
pub use video::{
    codec_tag_transcode_reason, is_hdr, needs_video_transcode, open_gop_transcode_reason, video_transcode_reason,
};
