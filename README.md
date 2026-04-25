# tokimo-package-hls

HLS transcoding session manager for Tokimo — adaptive bitrate streaming with hardware-accelerated pipelines.

## Features

- **Session management** — create, destroy, ping, and seek HLS sessions (`HlsSessionManager`)
- **Hardware acceleration** — NVENC (CUDA), VAAPI, QSV (Intel), RKMPP (Rockchip), VideoToolbox (macOS), AMF (AMD), with automatic fallback to software
- **Transcode decisions** — smart codec and container selection based on client capabilities and source stream
- **Seek optimization** — seek-restart vs. segment skip logic (matches Jellyfin `TranscodeManager`)
- **Matroska helpers** — embedded subtitle extraction, forced-track detection
- **Idle cleanup** — auto-stop sessions after 60 s of inactivity
- **Built on `tokimo-package-ffmpeg`** — delegates actual encoding to the ffmpeg integration layer

## Usage

```rust
use tokimo_package_hls::{HlsSessionManager, CreateSessionRequest};
use std::sync::Arc;

let manager = Arc::new(HlsSessionManager::new());

let req = CreateSessionRequest {
    file_id: "abc123".into(),
    file_path: "/media/movie.mkv".into(),
    audio_stream_index: Some(0),
    ..Default::default()
};

let info = manager.create_session(req, "http://localhost:5678").await?;
println!("Playlist: {}", info.playlist_url);

// Ping to keep session alive
manager.ping_session(&info.session_id).await;
```

## Cargo

```toml
tokimo-package-hls = { git = "https://github.com/tokimo-lab/tokimo-package-hls" }
```

## License

MIT
