/// Matroska (.mkv) Cues index reader.
///
/// Extracts keyframe positions from the Cues element without scanning the full
/// file. Matches Jellyfin's `MatroskaKeyframeExtractor` approach:
/// the Cues element is a compact seek index (typically 200–600 KB) located near
/// the end of the file, so the whole extraction takes a few milliseconds even
/// for 50 GB files.
///
/// Supports both local filesystem paths and remote VFS sources (SMB, SFTP, …)
/// via a `DirectInput` callback — the same abstraction used by FFmpeg AVIO.
///
/// Returns keyframe times in seconds. Returns an error when the file has no
/// Cues element, is not a Matroska container, or cannot be read.
use std::io;
use std::sync::Arc;

use tokimo_package_ffmpeg::ReadAt;

// ── EBML element IDs ────────────────────────────────────────────────────────
const ID_EBML: u64 = 0x1A45_DFA3;
const ID_SEGMENT: u64 = 0x1853_8067;
const ID_SEEKHEAD: u64 = 0x114D_9B74;
const ID_SEEK: u64 = 0x4DBB;
const ID_SEEK_ID: u64 = 0x53AB;
const ID_SEEK_POSITION: u64 = 0x53AC;
const ID_INFO: u64 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u64 = 0x002A_D7B1;
const ID_CUES: u64 = 0x1C53_BB6B;
const ID_CUE_POINT: u64 = 0xBB;
const ID_CUE_TIME: u64 = 0xB3;

/// Default Matroska timestamp scale: 1 ms per tick (in nanoseconds).
const DEFAULT_TIMESTAMP_SCALE_NS: u64 = 1_000_000;

/// Read-ahead chunk size.  Amortises per-call overhead for both local files
/// and remote VFS sources (SMB, SFTP …).  64 KB covers the full SeekHead and
/// many Cues CuePoint entries in a single underlying read.
const READAHEAD: usize = 64 * 1024;

// ── Cursor — position-tracking buffered reader over any `read_at` source ────

/// Thin buffered cursor on top of an arbitrary `read_at(offset, size)` source.
///
/// Maintains an internal 64 KB read-ahead window so that the many small EBML
/// field reads (1–8 bytes each) do not each trigger a separate VFS round-trip.
struct Cursor {
    /// `read_at(offset, max_bytes) → Ok(Vec<u8>)`.  May return fewer bytes at
    /// EOF.  The implementation may be a syscall wrapper (local file) or a VFS
    /// callback (SMB/SFTP via `DirectInput`).
    read_at: ReadAt,
    /// Current logical position.
    pos: u64,
    /// Cached read-ahead data.
    buf: Vec<u8>,
    /// File offset that `buf[0]` corresponds to.
    buf_start: u64,
}

impl Cursor {
    fn new(read_at: ReadAt) -> Self {
        Self {
            read_at,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    /// Build a `Cursor` that reads from a local filesystem file.
    fn from_local(path: &str) -> io::Result<Self> {
        use std::fs::File;
        use std::os::unix::fs::FileExt;

        let file = Arc::new(File::open(path)?);
        let read_at: ReadAt = Arc::new(move |offset: u64, size: usize| -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; size];
            let n = file.read_at(&mut buf, offset)?;
            buf.truncate(n);
            Ok(buf)
        });
        Ok(Self::new(read_at))
    }

    /// Build a `Cursor` that reads through a `DirectInput` VFS callback
    /// (used for remote sources: SMB, SFTP, S3, FTP …).
    fn from_direct_input(input: &tokimo_package_ffmpeg::DirectInput) -> Self {
        Self::new(input.read_at.clone())
    }

    /// Ensure `pos..pos+needed` is in `buf`, fetching if necessary.
    fn fill(&mut self, needed: usize) -> io::Result<()> {
        let end_needed = self.pos + needed as u64;
        let buf_end = self.buf_start + self.buf.len() as u64;
        if self.pos >= self.buf_start && end_needed <= buf_end {
            return Ok(());
        }
        let fetch = needed.max(READAHEAD);
        let data = (self.read_at)(self.pos, fetch)?;
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
        self.buf_start = self.pos;
        self.buf = data;
        Ok(())
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        self.fill(1)?;
        let b = self.buf[(self.pos - self.buf_start) as usize];
        self.pos += 1;
        Ok(b)
    }

    fn read_exact_buf(&mut self, n: usize) -> io::Result<Vec<u8>> {
        self.fill(n)?;
        let start = (self.pos - self.buf_start) as usize;
        let end = start + n;
        if end > self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF in read_exact_buf"));
        }
        let out = self.buf[start..end].to_vec();
        self.pos += n as u64;
        Ok(out)
    }

    fn seek_to(&mut self, target: u64) {
        // Invalidate buffer only when moving outside the current window.
        if target < self.buf_start || target >= self.buf_start + self.buf.len() as u64 {
            self.buf.clear();
            self.buf_start = target;
        }
        self.pos = target;
    }

    fn pos(&self) -> u64 {
        self.pos
    }
}

// ── Low-level EBML primitives (operate on Cursor) ───────────────────────────

/// Read an EBML element ID (variable width 1–4 bytes; raw bytes form the ID).
fn read_id(c: &mut Cursor) -> io::Result<u64> {
    let first = c.read_u8()?;
    let width: usize = if first & 0x80 != 0 {
        1
    } else if first & 0x40 != 0 {
        2
    } else if first & 0x20 != 0 {
        3
    } else if first & 0x10 != 0 {
        4
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid EBML ID first byte"));
    };
    let mut id = u64::from(first);
    for _ in 1..width {
        id = (id << 8) | u64::from(c.read_u8()?);
    }
    Ok(id)
}

/// Read an EBML VINT size.  Returns `None` for the "unknown size" marker.
fn read_size(c: &mut Cursor) -> io::Result<Option<u64>> {
    let first = c.read_u8()?;
    let (width, mask_bits): (usize, u64) = if first & 0x80 != 0 {
        (1, 0x7F)
    } else if first & 0x40 != 0 {
        (2, 0x3F)
    } else if first & 0x20 != 0 {
        (3, 0x1F)
    } else if first & 0x10 != 0 {
        (4, 0x0F)
    } else if first & 0x08 != 0 {
        (5, 0x07)
    } else if first & 0x04 != 0 {
        (6, 0x03)
    } else if first & 0x02 != 0 {
        (7, 0x01)
    } else if first & 0x01 != 0 {
        (8, 0x00)
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid EBML size byte"));
    };
    let mut size = u64::from(first) & mask_bits;
    for _ in 1..width {
        size = (size << 8) | u64::from(c.read_u8()?);
    }
    let unknown = (1u64 << (7 * width)) - 1;
    if size == unknown { Ok(None) } else { Ok(Some(size)) }
}

/// Read `n` bytes as a big-endian unsigned integer.
fn read_uint(c: &mut Cursor, n: u64) -> io::Result<u64> {
    if n > 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "uint element too wide"));
    }
    let mut val = 0u64;
    for _ in 0..n {
        val = (val << 8) | u64::from(c.read_u8()?);
    }
    Ok(val)
}

/// Read `n` bytes as raw bytes (for SeekID binary element).
fn read_binary(c: &mut Cursor, n: u64) -> io::Result<Vec<u8>> {
    if n > 64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "binary element too large"));
    }
    c.read_exact_buf(n as usize)
}

/// Skip `n` bytes forward.
fn skip(c: &mut Cursor, n: u64) {
    c.seek_to(c.pos() + n);
}

fn pos(c: &Cursor) -> u64 {
    c.pos()
}

// ── SeekHead parsing ─────────────────────────────────────────────────────────

struct SeekEntry {
    id: u64,
    position: u64,
}

fn parse_seekhead(c: &mut Cursor, body_end: u64) -> io::Result<Vec<SeekEntry>> {
    let mut entries = Vec::new();

    while pos(c) < body_end {
        let id = read_id(c)?;
        let size = read_size(c)?.unwrap_or(0);
        let elem_end = pos(c) + size;

        if id == ID_SEEK {
            let mut seek_id_bytes: Option<Vec<u8>> = None;
            let mut seek_pos: Option<u64> = None;

            while pos(c) < elem_end {
                let cid = read_id(c)?;
                let csz = read_size(c)?.unwrap_or(0);
                match cid {
                    ID_SEEK_ID => {
                        seek_id_bytes = Some(read_binary(c, csz)?);
                    }
                    ID_SEEK_POSITION => {
                        seek_pos = Some(read_uint(c, csz)?);
                    }
                    _ => {
                        skip(c, csz);
                    }
                }
            }

            if let (Some(id_bytes), Some(p)) = (seek_id_bytes, seek_pos) {
                let mut id_val = 0u64;
                for &b in &id_bytes {
                    id_val = (id_val << 8) | u64::from(b);
                }
                entries.push(SeekEntry {
                    id: id_val,
                    position: p,
                });
            }
        } else {
            skip(c, size);
        }

        if pos(c) > elem_end {
            c.seek_to(elem_end);
        }
    }
    Ok(entries)
}

// ── Info parsing (TimestampScale) ────────────────────────────────────────────

fn parse_info_timestamp_scale(c: &mut Cursor, body_end: u64) -> io::Result<u64> {
    while pos(c) < body_end {
        let id = read_id(c)?;
        let size = read_size(c)?.unwrap_or(0);
        if id == ID_TIMESTAMP_SCALE {
            return read_uint(c, size);
        }
        skip(c, size);
    }
    Ok(DEFAULT_TIMESTAMP_SCALE_NS)
}

// ── Cues parsing ─────────────────────────────────────────────────────────────

fn parse_cues(c: &mut Cursor, body_end: u64) -> io::Result<Vec<u64>> {
    let mut times = Vec::new();

    while pos(c) < body_end {
        let id = read_id(c)?;
        let size = read_size(c)?.unwrap_or(0);
        let elem_end = pos(c) + size;

        if id == ID_CUE_POINT {
            let mut cue_time: Option<u64> = None;
            while pos(c) < elem_end {
                let cid = read_id(c)?;
                let csz = read_size(c)?.unwrap_or(0);
                if cid == ID_CUE_TIME {
                    cue_time = Some(read_uint(c, csz)?);
                } else {
                    skip(c, csz);
                }
                if cue_time.is_some() {
                    c.seek_to(elem_end);
                    break;
                }
            }
            if let Some(t) = cue_time {
                times.push(t);
            }
        } else {
            skip(c, size);
        }

        if pos(c) > elem_end {
            c.seek_to(elem_end);
        }
    }
    Ok(times)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract keyframe positions (in seconds) from a local Matroska file using its
/// Cues index.
///
/// Reads only the SeekHead (~16 KB) and the Cues element (~500 KB); does not
/// scan the full file.  Takes ~2 ms even for 50 GB files.
pub fn extract_keyframes(path: &str) -> Result<Vec<f64>, String> {
    let c = Cursor::from_local(path).map_err(|e| format!("open {path}: {e}"))?;
    extract_from_cursor(c)
}

/// Extract keyframe positions (in seconds) from a remote Matroska source via a
/// `DirectInput` VFS callback (SMB, SFTP, S3, FTP …).
///
/// Uses the same buffered EBML parser as `extract_keyframes`; reads at most
/// ~516 KB of data via the callback regardless of file size.
pub fn extract_keyframes_vfs(input: &tokimo_package_ffmpeg::DirectInput) -> Result<Vec<f64>, String> {
    let c = Cursor::from_direct_input(input);
    extract_from_cursor(c)
}

/// Core extraction logic shared by both entry points.
fn extract_from_cursor(mut c: Cursor) -> Result<Vec<f64>, String> {
    // ── 1. EBML header ───────────────────────────────────────────────────────
    let id = read_id(&mut c).map_err(|e| e.to_string())?;
    if id != ID_EBML {
        return Err("not a Matroska file (missing EBML header)".into());
    }
    let header_size = read_size(&mut c).map_err(|e| e.to_string())?.unwrap_or(0);
    skip(&mut c, header_size);

    // ── 2. Segment element ───────────────────────────────────────────────────
    let seg_id = read_id(&mut c).map_err(|e| e.to_string())?;
    if seg_id != ID_SEGMENT {
        return Err("expected Segment element".into());
    }
    let _seg_size = read_size(&mut c).map_err(|e| e.to_string())?;
    let segment_data_start = pos(&c);

    // ── 3. Scan top-level Segment children for SeekHead ─────────────────────
    let mut cues_offset: Option<u64> = None;
    let mut info_offset: Option<u64> = None;
    let mut timestamp_scale = DEFAULT_TIMESTAMP_SCALE_NS;

    let scan_limit = segment_data_start + 128 * 1024;

    'outer: while pos(&c) < scan_limit {
        let Ok(id) = read_id(&mut c) else { break };
        let Ok(Some(size)) = read_size(&mut c) else { break };
        let elem_end = pos(&c) + size;

        match id {
            ID_SEEKHEAD => {
                let entries = parse_seekhead(&mut c, elem_end).map_err(|e| e.to_string())?;
                for entry in &entries {
                    if entry.id == ID_CUES {
                        cues_offset = Some(segment_data_start + entry.position);
                    }
                    if entry.id == ID_INFO {
                        info_offset = Some(segment_data_start + entry.position);
                    }
                }
                if cues_offset.is_some() {
                    break 'outer;
                }
            }
            ID_INFO => {
                timestamp_scale = parse_info_timestamp_scale(&mut c, elem_end).map_err(|e| e.to_string())?;
            }
            ID_CUES => {
                let ticks = parse_cues(&mut c, elem_end).map_err(|e| e.to_string())?;
                return ticks_to_secs(ticks, timestamp_scale);
            }
            _ => {}
        }
        c.seek_to(elem_end);
    }

    // ── 4. Read Info for TimestampScale if we have a separate offset ─────────
    if let Some(info_off) = info_offset {
        c.seek_to(info_off);
        let _id = read_id(&mut c).map_err(|e| e.to_string())?;
        let size = read_size(&mut c).map_err(|e| e.to_string())?.unwrap_or(0);
        let body_end = pos(&c) + size;
        timestamp_scale = parse_info_timestamp_scale(&mut c, body_end).map_err(|e| e.to_string())?;
    }

    // ── 5. Seek to Cues and parse ────────────────────────────────────────────
    let cues_off = cues_offset.ok_or_else(|| "no Cues element found in SeekHead".to_string())?;
    c.seek_to(cues_off);

    let id = read_id(&mut c).map_err(|e| e.to_string())?;
    if id != ID_CUES {
        return Err(format!("expected Cues element at offset {cues_off}, got id=0x{id:X}"));
    }
    let cues_size = read_size(&mut c).map_err(|e| e.to_string())?.unwrap_or(u64::MAX / 2);
    let body_end = pos(&c) + cues_size;

    let ticks = parse_cues(&mut c, body_end).map_err(|e| e.to_string())?;
    ticks_to_secs(ticks, timestamp_scale)
}

fn ticks_to_secs(ticks: Vec<u64>, scale_ns: u64) -> Result<Vec<f64>, String> {
    if ticks.is_empty() {
        return Err("Cues element contains no CuePoint entries".into());
    }
    // scale_ns / 1e9 converts ticks → seconds.
    let scale = scale_ns as f64 / 1_000_000_000.0;
    let mut secs: Vec<f64> = ticks.iter().map(|&t| t as f64 * scale).collect();
    secs.sort_by(f64::total_cmp);
    Ok(secs)
}
