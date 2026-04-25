use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use tokimo_package_ffmpeg::transcode::{self, CancellationToken, HlsSegmentType, PauseToken, SeekCommand};
use tokimo_package_ffmpeg::{HwAccel, HwType};

use crate::ffmpeg::{SEGMENT_DURATION, build_transcode_options, generate_vod_playlist};
use crate::matroska;
use crate::types::{CreateSessionRequest, PlaybackSnapshot, SessionState};

/// How many segments ahead of current progress triggers an `FFmpeg` seek-restart.
/// Jellyfin: `segmentGapRequiringTranscodingChange = 24 / state.SegmentLength`.
/// For `SEGMENT_DURATION=6` → 4 segments.
const SEEK_THRESHOLD_SEGMENTS: u32 = 24 / SEGMENT_DURATION;

/// Throttle check interval — matches Jellyfin `TranscodingThrottler` timer (5 000 ms).
const THROTTLE_CHECK_INTERVAL_SECS: u64 = 5;

/// Max seconds `FFmpeg` may run ahead of the client's download position before
/// being paused.  Matches Jellyfin: `Math.Max(ThrottleDelaySeconds, 60)`.
const THROTTLE_AHEAD_SECS: u32 = 60;

/// Handles needed to wait for a segment file WITHOUT holding the session lock.
///
/// Obtained from `HlsSession::prepare_segment_wait`.  The caller drops the lock
/// before calling `SegmentWaitHandle::wait` so that concurrent stop / seek
/// requests are not blocked.
pub struct SegmentWaitHandle {
    pub path: PathBuf,
    /// Path of the next segment — used for readiness check (Jellyfin-style).
    pub next_path: Option<PathBuf>,
    segment_notify: Arc<Notify>,
    ffmpeg_exited: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    /// Highest segment number confirmed fully written (inotify `CLOSE_WRITE`).
    /// -1 means no segment has been confirmed yet (non-Linux or startup).
    last_completed: Arc<AtomicI64>,
    /// This handle's segment number, for comparison with `last_completed`.
    requested_segment: Option<u32>,
}

impl SegmentWaitHandle {
    /// Wait for the segment to appear on disk, up to 60 seconds.
    /// A segment is considered ready when it exists AND either:
    ///   - `FFmpeg` has exited (all existing files are complete), or
    ///   - The next segment also exists (proves this one is fully written).
    ///
    /// Matches Jellyfin's `GetDynamicSegment` readiness logic.
    /// Returns `Some(path)` when ready, `None` on timeout / session stopped /
    /// `FFmpeg` exited unexpectedly.
    pub async fn wait(self) -> Option<PathBuf> {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_mins(1);
        loop {
            if self.stopped.load(Ordering::Relaxed) {
                return None;
            }
            if self.is_ready() {
                return Some(self.path);
            }
            // If FFmpeg has already exited and the file doesn't exist, it never will.
            // Without this check we'd wait up to 60 s for a notify that was already
            // sent before this SegmentWaitHandle was created.
            if self.ffmpeg_exited.load(Ordering::Relaxed) {
                return if self.path.exists() { Some(self.path) } else { None };
            }
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                return if self.is_ready() { Some(self.path) } else { None };
            }
            tokio::select! {
                () = self.segment_notify.notified() => {
                    if self.is_ready() {
                        return Some(self.path);
                    }
                    // FFmpeg exited while we were waiting — check one last time.
                    if self.ffmpeg_exited.load(Ordering::Relaxed) {
                        return if self.path.exists() { Some(self.path) } else { None };
                    }
                }
                () = tokio::time::sleep(timeout) => {
                    return if self.is_ready() { Some(self.path) } else { None };
                }
            }
        }
    }

    /// Check if the segment is ready to serve.
    /// Matches Jellyfin: segment exists AND (job exited OR next segment exists).
    /// Enhanced: also considers inotify `CLOSE_WRITE` confirmation (Linux).
    fn is_ready(&self) -> bool {
        if !self.path.exists() {
            return false;
        }
        // FFmpeg exited — all existing files are complete.
        if self.ffmpeg_exited.load(Ordering::Relaxed) {
            return true;
        }
        // Segment confirmed fully written by inotify CLOSE_WRITE —
        // no need to wait for the next segment to appear.
        if let Some(seg) = self.requested_segment
            && self.last_completed.load(Ordering::Relaxed) >= i64::from(seg)
        {
            return true;
        }
        // init.mp4 now uses first media segment as next_path, so this only
        // triggers for truly non-parseable names.  File existence suffices.
        let Some(next) = &self.next_path else {
            return true;
        };
        next.exists()
    }
}

/// One active HLS transcoding session.
///
/// Owns:
///  - A temporary directory for HLS segments and playlist
///  - An FFI transcode task (via `spawn_blocking`)
///  - Cancel / pause tokens for lifecycle control
pub struct HlsSession {
    pub id: String,
    pub file_id: String,
    pub audio_stream_index: u32,
    pub duration_secs: f64,
    pub output_dir: PathBuf,
    pub state: SessionState,
    /// Pre-generated VOD playlist content (full seeking from the start).
    pub vod_playlist: String,
    /// Per-segment start times in seconds derived from Matroska Cues.
    /// Empty for equal-length-segment mode (unindexed .ts and transcode mode).
    /// When non-empty, `segment_start_times[i]` is the exact keyframe time
    /// that marks the start of segment `i`; used for precise seek restarts.
    segment_start_times: Vec<f64>,
    /// Segment container format for this session.
    pub segment_type: HlsSegmentType,
    /// Handle to the persistent worker thread (lives for the entire session).
    worker_handle: Option<JoinHandle<()>>,
    /// Send seek commands to the persistent worker.
    seek_tx: Option<std::sync::mpsc::SyncSender<SeekCommand>>,
    /// Cancel the current transcode pass.
    cancel_token: CancellationToken,
    /// Pause/resume the FFI transcode loop (replaces SIGSTOP/SIGCONT).
    pause_token: PauseToken,
    stopped: Arc<AtomicBool>,
    /// Set to true when the current transcode pass finishes (success or EOF).
    /// Reset to false on seek-restart. Shared with the persistent worker.
    ffmpeg_exited: Arc<AtomicBool>,
    /// Notified when a new segment file appears OR when a transcode pass exits.
    /// Shared with the persistent worker (not recreated on seek-restart).
    segment_notify: Arc<Notify>,
    /// Highest consecutive segment count from `ffmpeg_start_segment`.
    segment_count: Arc<AtomicU32>,
    /// The `start_number` of the current transcode (where it began generating).
    ffmpeg_start_segment: u32,
    /// Original request — kept for seek-restart.
    original_request: CreateSessionRequest,
    /// Highest segment index the client has requested (for throttling).
    download_position: Arc<AtomicU32>,
    /// Highest segment number confirmed fully written by inotify `CLOSE_WRITE`.
    /// -1 means no segment completed yet. Used for faster segment readiness
    /// without waiting for the next segment file to appear.
    last_completed_segment: Arc<AtomicI64>,
    // ── Playback tracking metadata ──
    user_id: Option<String>,
    video_item_id: Option<String>,
    episode_id: Option<String>,
}

impl HlsSession {
    /// Create and start a new HLS session.
    pub async fn start(id: String, req: &CreateSessionRequest) -> Result<Self, String> {
        let output_dir = std::env::temp_dir().join(format!("tokimo-hls/{id}"));
        tokio::fs::create_dir_all(&output_dir)
            .await
            .map_err(|e| format!("Failed to create HLS temp dir: {e}"))?;

        let segment_type = if req.transcode_video {
            HlsSegmentType::Fmp4
        } else {
            HlsSegmentType::Mpegts
        };

        // For .mkv files in copy-video mode, extract keyframes from the
        // Matroska Cues index (fast: reads ~500 KB index, not the full file).
        // This gives us accurate per-segment start times so seeks land exactly
        // on keyframe boundaries — matching Jellyfin's ComputeSegments approach.
        //
        // Supports both local filesystem files and remote VFS sources (SMB,
        // SFTP, S3 …) via the DirectInput callback used by FFmpeg AVIO.
        let is_mkv = |hint: &str| hint.to_ascii_lowercase().ends_with(".mkv");

        let keyframes: Option<Vec<f64>> =
            if req.transcode_video {
                None
            } else {
                let from_local = req.local_path.as_deref().filter(|p| is_mkv(p)).and_then(|p| {
                    match matroska::extract_keyframes(p) {
                        Ok(kfs) => {
                            info!("[HLS] Matroska Cues (local): {} keyframes", kfs.len());
                            Some(kfs)
                        }
                        Err(e) => {
                            info!("[HLS] Matroska Cues (local) skipped: {e}");
                            None
                        }
                    }
                });

                let from_vfs = if from_local.is_none() {
                    if let Some(di) = req.direct_input.clone() {
                        let hint = di.filename_hint.as_deref().unwrap_or("");
                        if is_mkv(hint) {
                            // DirectInput.read_at uses Handle::block_on internally,
                            // which panics on a tokio worker thread. Run on a
                            // dedicated blocking thread so the nested block_on is safe.
                            match tokio::task::spawn_blocking(move || matroska::extract_keyframes_vfs(&di)).await {
                                Ok(Ok(kfs)) => {
                                    info!("[HLS] Matroska Cues (VFS): {} keyframes", kfs.len());
                                    Some(kfs)
                                }
                                Ok(Err(e)) => {
                                    info!("[HLS] Matroska Cues (VFS) skipped: {e}");
                                    None
                                }
                                Err(e) => {
                                    info!("[HLS] Matroska Cues (VFS) spawn_blocking failed: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                from_local.or(from_vfs)
            };

        let (vod_playlist, segment_start_times) =
            generate_vod_playlist(req.duration_secs, SEGMENT_DURATION, segment_type, keyframes.as_deref());

        let cancel_token = transcode::cancellation_token();
        let pause_token = transcode::pause_token();

        // Get the pooled device context for this HW backend (created once per process,
        // shared across all sessions via refcounted clone).
        let cached_device_ctx = if req.transcode_video {
            HwAccel::get_or_create_device_ctx(HwType::Cuda)
        } else {
            None
        };

        let opts = build_transcode_options(
            req.local_path.as_deref(),
            req.audio_stream_index,
            &req.audio_streams,
            output_dir.to_str().unwrap(),
            SEGMENT_DURATION,
            None,
            0,
            req.transcode_video,
            req.transcode_audio,
            req.target_audio_codec.as_deref(),
            req.tonemap.as_ref(),
            req.video_codec.as_deref(),
            req.video_width,
            req.video_height,
            req.video_fps,
            req.video_bitrate,
            req.deinterlace,
            req.client_supports_hevc,
            Some(cancel_token.clone()),
            Some(pause_token.clone()),
            cached_device_ctx.clone(),
            req.direct_input.clone(),
            req.iso_type.as_deref(),
        );

        let stopped = Arc::new(AtomicBool::new(false));
        let segment_notify = Arc::new(Notify::new());
        let segment_count = Arc::new(AtomicU32::new(0));
        let ffmpeg_exited = Arc::new(AtomicBool::new(false));
        let download_position = Arc::new(AtomicU32::new(0));
        let last_completed_segment = Arc::new(AtomicI64::new(-1));

        // Channel for seek-restart commands to the persistent worker thread.
        let (seek_tx, seek_rx) = std::sync::mpsc::sync_channel::<SeekCommand>(1);

        let session_id_clone = id.clone();
        let exited_flag = ffmpeg_exited.clone();
        let exit_notify = segment_notify.clone();
        let stopped_flag = stopped.clone();

        // The on_pass_finish callback is called by the worker after each pass.
        // It shares the session-level ffmpeg_exited/segment_notify Arcs.
        let pass_exited = ffmpeg_exited.clone();
        let pass_notify = segment_notify.clone();
        let on_pass_finish = move |completed: bool| {
            if completed {
                pass_exited.store(true, Ordering::Relaxed);
            }
            pass_notify.notify_waiters();
        };

        let handle = tokio::task::spawn_blocking(move || {
            match transcode::transcode_session(&opts, seek_rx, on_pass_finish) {
                Ok(()) => {
                    if !stopped_flag.load(Ordering::Relaxed) {
                        info!("[HLS:{}] transcode session finished", session_id_clone);
                    }
                }
                Err(e) => {
                    if !stopped_flag.load(Ordering::Relaxed) {
                        warn!("[HLS:{}] transcode session error: {}", session_id_clone, e);
                    }
                }
            }
            exited_flag.store(true, Ordering::Relaxed);
            exit_notify.notify_waiters();
        });

        let session = Self {
            id: id.clone(),
            file_id: req.file_id.clone(),
            audio_stream_index: req.audio_stream_index,
            duration_secs: req.duration_secs,
            output_dir: output_dir.clone(),
            state: SessionState::Running,
            vod_playlist,
            segment_start_times,
            segment_type,
            worker_handle: Some(handle),
            seek_tx: Some(seek_tx),
            cancel_token,
            pause_token,
            stopped: stopped.clone(),
            ffmpeg_exited: ffmpeg_exited.clone(),
            segment_notify: segment_notify.clone(),
            segment_count: segment_count.clone(),
            ffmpeg_start_segment: 0,
            original_request: req.clone(),
            download_position: download_position.clone(),
            last_completed_segment: last_completed_segment.clone(),
            user_id: req.user_id.clone(),
            video_item_id: req.video_item_id.clone(),
            episode_id: req.episode_id.clone(),
        };

        Self::spawn_background_tasks(
            &session,
            id,
            stopped,
            segment_notify,
            segment_count,
            output_dir,
            0,
            download_position,
            session.pause_token.clone(),
            segment_type,
            last_completed_segment,
        );

        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_background_tasks(
        session: &Self,
        session_id: String,
        stopped: Arc<AtomicBool>,
        segment_notify: Arc<Notify>,
        segment_count: Arc<AtomicU32>,
        output_dir: PathBuf,
        start_segment: u32,
        download_position: Arc<AtomicU32>,
        pause_token: PauseToken,
        segment_type: HlsSegmentType,
        last_completed_segment: Arc<AtomicI64>,
    ) {
        // Task 1: watch output dir for new segment files and notify waiters.
        let watch_dir = output_dir;
        let watch_stopped = stopped.clone();
        let watch_notify = segment_notify;
        let seg_ext = segment_type.extension().to_string();
        tokio::spawn(async move {
            Self::watch_segments(
                watch_dir,
                watch_stopped,
                watch_notify,
                segment_count,
                start_segment,
                &seg_ext,
                last_completed_segment,
            )
            .await;
        });

        // Task 2: Transcoding throttler — pauses/resumes via PauseToken.
        let throttle_stopped = stopped;
        let throttle_segment_count = session.segment_count.clone();
        let throttle_download = download_position;
        let throttle_paused = pause_token;
        let throttle_id = session_id;
        let threshold_segments = THROTTLE_AHEAD_SECS / SEGMENT_DURATION;
        tokio::spawn(async move {
            loop {
                if throttle_stopped.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(THROTTLE_CHECK_INTERVAL_SECS)).await;
                if throttle_stopped.load(Ordering::Relaxed) {
                    break;
                }

                let transcoding_pos = start_segment + throttle_segment_count.load(Ordering::Relaxed);
                let download_pos = throttle_download.load(Ordering::Relaxed);
                let gap = transcoding_pos.saturating_sub(download_pos);

                if gap > threshold_segments {
                    if !throttle_paused.swap(true, Ordering::Relaxed) {
                        info!("[HLS:{}] Throttle: pause (ahead by {} segments)", throttle_id, gap);
                    }
                } else if throttle_paused.swap(false, Ordering::Relaxed) {
                    info!("[HLS:{}] Throttle: resume (gap={})", throttle_id, gap);
                }
            }
        });
    }

    /// Watch the output directory for new segment files using the platform's
    /// native file-system events (inotify on Linux, `FSEvents` on macOS, etc.)
    /// via the `notify` crate. Falls back to polling if the watcher fails to
    /// initialize.  Notifies waiters immediately when segments appear or are
    /// fully written, instead of waiting for the 150 ms polling interval.
    async fn watch_segments(
        dir: PathBuf,
        stopped: Arc<AtomicBool>,
        notify: Arc<Notify>,
        segment_count: Arc<AtomicU32>,
        start_segment: u32,
        ext: &str,
        last_completed: Arc<AtomicI64>,
    ) {
        use notify::{
            RecursiveMode, Watcher,
            event::{AccessKind, AccessMode, CreateKind},
        };
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<notify::Event>(64);
        let mut watcher = match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                warn!("[HLS] notify watcher init failed ({e}), falling back to polling");
                return Self::watch_segments_poll(dir, stopped, notify, segment_count, start_segment, ext).await;
            }
        };

        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            warn!("[HLS] notify watch failed ({e}), falling back to polling");
            return Self::watch_segments_poll(dir, stopped, notify, segment_count, start_segment, ext).await;
        }

        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { break };

                    let is_close_write = matches!(
                        event.kind,
                        notify::EventKind::Access(AccessKind::Close(AccessMode::Write))
                    );
                    let is_create = matches!(
                        event.kind,
                        notify::EventKind::Create(CreateKind::File | CreateKind::Any)
                    );

                    if is_close_write {
                        for path in &event.paths {
                            if let Some(name_str) = path.file_name().and_then(|n| n.to_str())
                                && let Some(num) = parse_segment_number(name_str, ext) {
                                    last_completed.fetch_max(i64::from(num), Ordering::Relaxed);
                                }
                        }
                    }

                    if is_create || is_close_write {
                        let count = count_segments_from(&dir, start_segment, ext);
                        let prev = segment_count.swap(count, Ordering::Relaxed);
                        // Notify on new segments OR on close-write (segment finalized).
                        if count != prev || is_close_write {
                            notify.notify_waiters();
                        }
                    }
                }
                () = tokio::time::sleep(tokio::time::Duration::from_secs(2)) => {
                    // Periodic fallback check in case we miss an event.
                    let count = count_segments_from(&dir, start_segment, ext);
                    segment_count.store(count, Ordering::Relaxed);
                    notify.notify_waiters();
                }
            }
        }
    }

    /// Polling fallback for segment watching (non-Linux or inotify failure).
    async fn watch_segments_poll(
        dir: PathBuf,
        stopped: Arc<AtomicBool>,
        notify: Arc<Notify>,
        segment_count: Arc<AtomicU32>,
        start_segment: u32,
        ext: &str,
    ) {
        let mut known_count = segment_count.load(Ordering::Relaxed);
        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
            let count = count_segments_from(&dir, start_segment, ext);
            if count != known_count {
                known_count = count;
                segment_count.store(count, Ordering::Relaxed);
                notify.notify_waiters();
            }
        }
    }

    /// Return a snapshot of the current playback state for progress persistence.
    pub fn playback_snapshot(&self) -> PlaybackSnapshot {
        let dl_pos = self.download_position.load(Ordering::Relaxed);
        PlaybackSnapshot {
            session_id: self.id.clone(),
            file_id: self.file_id.clone(),
            user_id: self.user_id.clone(),
            video_item_id: self.video_item_id.clone(),
            episode_id: self.episode_id.clone(),
            duration_secs: self.duration_secs,
            position_secs: f64::from(dl_pos) * f64::from(SEGMENT_DURATION),
            client_name: "HLS Server",
        }
    }

    /// Stop the session: cancel transcode and clean up temp files.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return; // Already stopped
        }
        self.state = SessionState::Stopped;

        // Cancel the current transcode pass — it will drain and exit cleanly.
        self.cancel_token.store(true, Ordering::Relaxed);
        // Unpause so the cancel check runs immediately.
        self.pause_token.store(false, Ordering::Relaxed);

        // Tell the persistent worker to shut down (it will exit after the current pass).
        if let Some(tx) = self.seek_tx.take() {
            let _ = tx.send(SeekCommand::Stop);
        }

        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.await;
        }

        if self.output_dir.exists()
            && let Err(e) = tokio::fs::remove_dir_all(&self.output_dir).await
        {
            warn!("[HLS:{}] cleanup failed: {}", self.id, e);
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// **Phase 1** of segment serving — call this under the session lock.
    ///
    /// Performs any necessary seek-restart synchronously, then returns a
    /// `SegmentWaitHandle` that the caller can await **after releasing the lock**.
    /// This two-phase design keeps the lock held for at most a few hundred
    /// milliseconds (spawn + kill), avoiding the deadlock where `stop_session`
    /// is blocked waiting for a lock that a 60-second wait holds.
    pub async fn prepare_segment_wait(&mut self, segment_name: &str) -> Option<SegmentWaitHandle> {
        if self.is_stopped() {
            return None;
        }

        let ext = self.segment_type.extension();
        let is_init_segment = segment_name == "init.mp4";

        // Track download position and unpause FFmpeg for ALL segment requests
        // (not just when the file is missing).  The fast-path below returns
        // early for segments already on disk — without this, the throttle
        // never sees the client catching up and FFmpeg stays paused.
        if !is_init_segment && let Some(requested_seg) = parse_segment_number(segment_name, ext) {
            self.download_position.fetch_max(requested_seg, Ordering::Relaxed);

            // Immediately resume transcode if paused and the client is catching
            // up — avoids waiting up to 5 s for the next throttle check.
            if self.pause_token.load(Ordering::Relaxed) {
                let estimated_max = self.ffmpeg_start_segment + self.segment_count.load(Ordering::Relaxed);
                let gap = estimated_max.saturating_sub(requested_seg);
                let threshold = THROTTLE_AHEAD_SECS / SEGMENT_DURATION;
                if gap <= threshold && self.pause_token.swap(false, Ordering::Relaxed) {
                    info!(
                        "[HLS:{}] Throttle: early resume (client at seg {})",
                        self.id, requested_seg
                    );
                }
            }
        }

        let segment_path = self.output_dir.join(segment_name);

        // For init.mp4 (fmp4 init segment): wait until the first media segment
        // also exists.  FFmpeg creates init.mp4 during write_header(), but
        // inotify fires CREATE before the file is fully written.  Using the
        // first media segment as a readiness gate guarantees init.mp4 is
        // complete (write_header finishes before any segment is produced).
        let next_path = if is_init_segment {
            Some(
                self.output_dir
                    .join(format!("{:05}.{}", self.ffmpeg_start_segment, ext)),
            )
        } else {
            next_segment_path(segment_name, &self.output_dir, ext)
        };

        // Fast path: already on disk — still need readiness check.
        if segment_path.exists() {
            return Some(SegmentWaitHandle {
                path: segment_path,
                next_path,
                segment_notify: self.segment_notify.clone(),
                ffmpeg_exited: self.ffmpeg_exited.clone(),
                stopped: self.stopped.clone(),
                last_completed: self.last_completed_segment.clone(),
                requested_segment: parse_segment_number(segment_name, ext),
            });
        }

        if !is_init_segment {
            let requested_seg = parse_segment_number(segment_name, ext).unwrap_or(0);

            let estimated_max = self.ffmpeg_start_segment + self.segment_count.load(Ordering::Relaxed);

            let needs_seek = if self.ffmpeg_exited.load(Ordering::Relaxed) {
                info!("[HLS:{}] Seek: seg {} (ffmpeg exited)", self.id, requested_seg);
                true
            } else if requested_seg < self.ffmpeg_start_segment {
                info!(
                    "[HLS:{}] Seek: seg {} ← backward (start={})",
                    self.id, requested_seg, self.ffmpeg_start_segment
                );
                true
            } else if requested_seg > estimated_max + SEEK_THRESHOLD_SEGMENTS {
                info!(
                    "[HLS:{}] Seek: seg {} → forward (max={})",
                    self.id, requested_seg, estimated_max
                );
                true
            } else {
                false
            };

            if needs_seek && let Err(e) = self.seek_restart(requested_seg).await {
                warn!("[HLS:{}] seek-restart failed: {}", self.id, e);
                return None;
            }
        }

        let segment_path = self.output_dir.join(segment_name);
        // Reuse the same next_path logic (init.mp4 → first segment, media → N+1)
        let next_path = if is_init_segment {
            Some(
                self.output_dir
                    .join(format!("{:05}.{}", self.ffmpeg_start_segment, ext)),
            )
        } else {
            next_segment_path(segment_name, &self.output_dir, ext)
        };

        // Return handles that the caller will poll WITHOUT holding the lock.
        Some(SegmentWaitHandle {
            path: segment_path,
            next_path,
            segment_notify: self.segment_notify.clone(),
            ffmpeg_exited: self.ffmpeg_exited.clone(),
            stopped: self.stopped.clone(),
            last_completed: self.last_completed_segment.clone(),
            requested_segment: parse_segment_number(segment_name, ext),
        })
    }

    /// Cancel current transcode pass and restart from a target segment position.
    /// The persistent worker thread stays alive — only the output pipeline is recreated.
    async fn seek_restart(&mut self, target_segment: u32) -> Result<(), String> {
        // Signal old background tasks to stop.
        self.stopped.store(true, Ordering::Relaxed);
        self.segment_notify.notify_waiters();

        // Cancel the current transcode pass and unpause so it exits promptly.
        self.cancel_token.store(true, Ordering::Relaxed);
        self.pause_token.store(false, Ordering::Relaxed);

        // Clean up stale segments.
        delete_segments_from(&self.output_dir, target_segment, self.segment_type.extension()).await;

        let raw_seek = f64::from(target_segment) * f64::from(SEGMENT_DURATION);

        let seek_secs = if raw_seek > 0.0 {
            let max_seek = (self.duration_secs - 5.0).max(0.0);

            let with_offset = if !self.segment_start_times.is_empty() {
                // Keyframe-indexed mode (Matroska Cues available).
                //
                // segment_start_times[N] is the exact source keyframe that
                // begins segment N.  Adding 0.1 s provides the same tiny
                // forward nudge as Jellyfin's +0.5 s (EncodingHelper.cs:2952):
                // avformat_seek_file with max_ts = target finds the keyframe
                // at or just before the target, landing precisely on
                // segment_start_times[N].  The keyframe gate in demux.rs then
                // clears immediately with zero gap.
                let exact = self
                    .segment_start_times
                    .get(target_segment as usize)
                    .copied()
                    .unwrap_or(raw_seek);
                exact + 0.1
            } else if self.original_request.transcode_video {
                // Transcode mode: FFmpeg inserts keyframes on its own schedule;
                // equal-length segments are accurate.
                raw_seek
            } else {
                // Copy-video mode, no Cues index (e.g. unindexed MPEG-TS).
                // Byte-offset estimation may land a keyframe *after* the target
                // boundary → buffer gap > maxBufferHole → hls.js stall.
                // Seeking one segment early guarantees the keyframe gate clears
                // before the boundary, turning any gap into an overlap.
                (raw_seek - f64::from(SEGMENT_DURATION)).max(0.0)
            };

            with_offset.clamp(0.0, max_seek)
        } else {
            0.0
        };

        // Fresh tokens for the new transcode pass.
        let cancel_token = transcode::cancellation_token();
        let pause_token = transcode::pause_token();

        info!(
            "[HLS:{}] Seek restart → {:.0}s (seg {})",
            self.id, seek_secs, target_segment
        );

        // Reset session-level pass-completion flag (shared with worker callback).
        self.ffmpeg_exited.store(false, Ordering::Relaxed);

        self.cancel_token = cancel_token.clone();
        self.pause_token = pause_token.clone();
        self.state = SessionState::Running;
        self.ffmpeg_start_segment = target_segment;

        // Send seek command to the persistent worker thread.
        // The worker will: seek in the existing demuxer → run a new transcode pass.
        if let Some(ref tx) = self.seek_tx {
            tx.send(SeekCommand::Seek {
                seek_secs,
                start_segment: target_segment,
                cancel: cancel_token,
                pause: pause_token.clone(),
            })
            .map_err(|_| format!("[HLS:{}] worker thread has exited", self.id))?;
        } else {
            return Err(format!("[HLS:{}] no worker thread available", self.id));
        }

        // Fresh Arcs for new background tasks (watcher + throttler).
        // These are separate from the session-level ffmpeg_exited/segment_notify
        // which are shared with the persistent worker.
        let stopped = Arc::new(AtomicBool::new(false));
        let segment_count = Arc::new(AtomicU32::new(0));
        let download_position = Arc::new(AtomicU32::new(target_segment));
        let last_completed_segment = Arc::new(AtomicI64::new(-1));

        self.stopped = stopped.clone();
        self.segment_count = segment_count.clone();
        self.download_position = download_position.clone();
        self.last_completed_segment = last_completed_segment.clone();

        Self::spawn_background_tasks(
            self,
            self.id.clone(),
            stopped,
            self.segment_notify.clone(),
            segment_count,
            self.output_dir.clone(),
            target_segment,
            download_position,
            pause_token,
            self.segment_type,
            last_completed_segment,
        );

        Ok(())
    }

    /// Check if the worker thread has finished (non-blocking).
    pub fn check_ffmpeg_status(&mut self) {
        if let Some(ref handle) = self.worker_handle
            && handle.is_finished()
            && self.ffmpeg_exited.load(Ordering::Relaxed)
        {
            // Determine success based on cancel state.
            if self.cancel_token.load(Ordering::Relaxed) {
                self.state = SessionState::Stopped;
            } else {
                self.state = SessionState::Finished;
            }
        }
    }
}

impl Drop for HlsSession {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.cancel_token.store(true, Ordering::Relaxed);
        self.pause_token.store(false, Ordering::Relaxed);
        // Send Stop to the worker so it exits promptly.
        if let Some(tx) = self.seek_tx.take() {
            let _ = tx.send(SeekCommand::Stop);
        }
        // The JoinHandle will be dropped — spawn_blocking tasks run to
        // completion even if the handle is dropped, but the cancel token
        // + Stop command ensures a prompt exit.
    }
}

/// Count **consecutive** segment files starting from `start`.
/// Only counts segments that form an unbroken chain (start, start+1, …),
/// so leftover segments from previous seek-restarts beyond a gap are not
/// included — this prevents the throttle from seeing an inflated count
/// and incorrectly SIGSTOP-ing a freshly restarted `FFmpeg`.
fn count_segments_from(dir: &Path, start: u32, ext: &str) -> u32 {
    let mut count = 0u32;
    loop {
        let seg = dir.join(format!("{:05}.{}", start + count, ext));
        if seg.exists() {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn parse_segment_number(filename: &str, ext: &str) -> Option<u32> {
    let name = filename.strip_suffix(&format!(".{ext}"))?;
    name.parse().ok()
}

/// Given a segment name like "01560.ts", return the path of the next
/// segment ("01561.ts").  Returns `None` for non-segment files
/// (e.g. "init.mp4").
fn next_segment_path(segment_name: &str, output_dir: &Path, ext: &str) -> Option<PathBuf> {
    let n = parse_segment_number(segment_name, ext)?;
    Some(output_dir.join(format!("{:05}.{}", n + 1, ext)))
}

/// Delete all segment files with numbers >= `from_segment`.
/// Called during seek-restart to remove stale segments left by a previous
/// `FFmpeg` instance that would otherwise confuse the segment counter / throttle.
async fn delete_segments_from(dir: &Path, from_segment: u32, ext: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if let Some(n) = name.to_str().and_then(|s| parse_segment_number(s, ext))
            && n >= from_segment
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}
