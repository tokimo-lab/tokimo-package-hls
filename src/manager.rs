use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::session::HlsSession;
use crate::types::{CreateSessionRequest, HlsSessionInfo, PlaybackSnapshot};

/// Kill-timer delay: if no segment is requested for this long, stop the session.
/// Matches Jellyfin's `TranscodeManager.PingTimer`: 60s for HLS.
const IDLE_TIMEOUT_SECS: u64 = 60;

/// Manages all active HLS transcoding sessions.
///
/// Thread-safe — designed to be shared via `Arc<HlsSessionManager>`.
pub struct HlsSessionManager {
    sessions: Mutex<HashMap<String, Arc<Mutex<HlsSession>>>>,
    /// Track last access time per session for idle cleanup.
    last_access: Mutex<HashMap<String, tokio::time::Instant>>,
    /// Lookup index: `session_id` → `file_id` (avoids locking inner session just to read `file_id`).
    file_ids: Mutex<HashMap<String, String>>,
}

impl HlsSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            last_access: Mutex::new(HashMap::new()),
            file_ids: Mutex::new(HashMap::new()),
        }
    }

    /// Create a new HLS session. Returns the session info including playlist URL.
    pub async fn create_session(&self, req: CreateSessionRequest, base_url: &str) -> Result<HlsSessionInfo, String> {
        let session_id = uuid::Uuid::new_v4().to_string();

        // Stop any existing session for the same file + audio combo
        self.stop_session_for_file(&req.file_id).await;

        let file_id = req.file_id.clone();
        let session = HlsSession::start(session_id.clone(), &req).await?;
        let session = Arc::new(Mutex::new(session));

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(session_id.clone(), session);
        }
        {
            let mut last_access = self.last_access.lock().await;
            last_access.insert(session_id.clone(), tokio::time::Instant::now());
        }
        {
            let mut file_ids = self.file_ids.lock().await;
            file_ids.insert(session_id.clone(), file_id);
        }

        let playlist_url = format!("{base_url}/api/hls/{session_id}/playlist.m3u8");

        info!(
            "[HLS] Session {} created (file={}, audio={})",
            session_id, req.file_id, req.audio_stream_index
        );

        Ok(HlsSessionInfo {
            session_id,
            playlist_url,
        })
    }

    /// Get a session by ID, updating its last-access timestamp.
    pub async fn get_session(&self, session_id: &str) -> Option<Arc<Mutex<HlsSession>>> {
        let sessions = self.sessions.lock().await;
        let session = sessions.get(session_id)?.clone();

        // Update last access time
        let mut last_access = self.last_access.lock().await;
        last_access.insert(session_id.to_string(), tokio::time::Instant::now());

        Some(session)
    }

    /// Stop and remove a specific session. Returns the final playback snapshot.
    pub async fn stop_session(&self, session_id: &str) -> Option<PlaybackSnapshot> {
        let session = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id)
        };
        let snapshot = if let Some(session) = session {
            let mut s = session.lock().await;
            let snap = s.playback_snapshot();
            s.stop().await;
            info!("[HLS] stopped session {}", session_id);
            Some(snap)
        } else {
            None
        };
        {
            let mut last_access = self.last_access.lock().await;
            last_access.remove(session_id);
        }
        {
            let mut file_ids = self.file_ids.lock().await;
            file_ids.remove(session_id);
        }
        snapshot
    }

    /// Stop all sessions for a given file ID (used when creating a new session).
    pub async fn stop_session_for_file(&self, file_id: &str) {
        let to_stop: Vec<String> = {
            let file_ids = self.file_ids.lock().await;
            file_ids
                .iter()
                .filter(|(_, fid)| fid.as_str() == file_id)
                .map(|(sid, _)| sid.clone())
                .collect()
        };
        for id in to_stop {
            self.stop_session(&id).await;
        }
    }

    /// Look up the `file_id` for a session without acquiring the session lock.
    pub async fn get_file_id(&self, session_id: &str) -> Option<String> {
        self.file_ids.lock().await.get(session_id).cloned()
    }

    /// Return playback snapshots for all active sessions.
    pub async fn playback_snapshots(&self) -> Vec<PlaybackSnapshot> {
        let session_arcs: Vec<Arc<Mutex<HlsSession>>> = {
            let sessions = self.sessions.lock().await;
            sessions.values().cloned().collect()
        };
        let mut snapshots = Vec::with_capacity(session_arcs.len());
        for session in &session_arcs {
            let s = session.lock().await;
            snapshots.push(s.playback_snapshot());
        }
        snapshots
    }

    /// Clean up idle sessions that haven't been accessed recently.
    /// Returns snapshots of stopped sessions for final progress persistence.
    pub async fn cleanup_idle_sessions(&self) -> Vec<PlaybackSnapshot> {
        let now = tokio::time::Instant::now();
        let idle_timeout = tokio::time::Duration::from_secs(IDLE_TIMEOUT_SECS);

        let to_stop: Vec<String> = {
            let last_access = self.last_access.lock().await;
            last_access
                .iter()
                .filter(|&(_, &last)| now.duration_since(last) > idle_timeout)
                .map(|(id, _)| id.clone())
                .collect()
        };

        let mut snapshots = Vec::new();
        for id in &to_stop {
            warn!("[HLS] stopping idle session {}", id);
            if let Some(snap) = self.stop_session(id).await {
                snapshots.push(snap);
            }
        }
        snapshots
    }

    /// Stop all sessions (used on server shutdown).
    pub async fn stop_all(&self) {
        let ids: Vec<String> = {
            let sessions = self.sessions.lock().await;
            sessions.keys().cloned().collect()
        };
        for id in ids {
            self.stop_session(&id).await;
        }
    }

    /// Start the background cleanup task.
    pub fn start_cleanup_task(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                manager.cleanup_idle_sessions().await;
            }
        });
    }
}

impl Default for HlsSessionManager {
    fn default() -> Self {
        Self::new()
    }
}
