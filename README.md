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

## Standalone development

This crate depends on `tokimo-package-ffmpeg`, which links against a patched
build of [jellyfin-ffmpeg](https://github.com/jellyfin/jellyfin-ffmpeg). When
hacking on this repo standalone (outside the [`tokimo.io`](https://github.com/tokimo-lab/tokimo.io)
monorepo, where it's resolved via a `[patch]` redirect), you need to provide
those native libraries yourself.

Easiest path:

```bash
# Pick your platform
PLATFORM=linux  # or macos / windows
mkdir -p .ffmpeg-install
cd .ffmpeg-install
gh release download nightly -R tokimo-lab/tokimo-package-ffmpeg \
  -p install-${PLATFORM}.tar.zst
tar --zstd -xf install-${PLATFORM}.tar.zst
cd ..

export FFMPEG_PKG_CONFIG_PATH=$PWD/.ffmpeg-install/install/lib/pkgconfig
export FFMPEG_INCLUDE_DIR=$PWD/.ffmpeg-install/install/include
export FFMPEG_DYN_DIR=$PWD/.ffmpeg-install/install/lib
export LD_LIBRARY_PATH=$FFMPEG_DYN_DIR  # or DYLD_FALLBACK_LIBRARY_PATH on macOS

cargo build
```

On macOS you'll additionally need the brew runtime dependencies the
ffmpeg install dylibs were linked against — see `.github/workflows/ci.yml`
for the full list. Inside the tokimo.io monorepo none of this is needed:
the workspace builds `tokimo-package-ffmpeg` from local source.
