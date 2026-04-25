pub mod ffmpeg;
pub mod hw_detect;
pub mod manager;
pub mod matroska;
pub mod pipeline;
pub mod session;
pub mod transcode_decision;
pub mod types;

pub use hw_detect::{HwCapabilities, get_hw_capabilities, resolve_ffmpeg_binary};
pub use manager::HlsSessionManager;
pub use session::{HlsSession, SegmentWaitHandle};
pub use types::{CreateSessionRequest, HlsSessionInfo, PlaybackSnapshot};
