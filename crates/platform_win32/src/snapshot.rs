//! Capture-on-hide window snapshots for the overview's `snapshot` render
//! mode: a `PrintWindow(PW_RENDERFULLCONTENT)` grab taken right before a
//! window leaves the screen (workspace switch, move-to-workspace), kept
//! in a small process-global LRU cache and stretched into overview card
//! bodies instead of registering live DWM thumbnails.
//!
//! Snapshots are downscaled (longest side capped at [`MAX_SIDE`] px,
//! HALFTONE) so 40 cached entries stay around ~25MB worst case.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC,
    SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;

/// `PW_RENDERFULLCONTENT` (Win 8.1+): asks DWM to render the window's
/// full composed content (DirectComposition / swap-chain surfaces
/// included) instead of just its GDI redirection surface. Not exposed by
/// the `windows` crate's constants.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

/// Longest snapshot side, px. Card bodies are far smaller; 480 keeps the
/// downscale crisp without caching megapixels per window.
const MAX_SIDE: i32 = 480;

/// Cache capacity; the oldest entry (by last capture) is evicted beyond it.
const MAX_ENTRIES: usize = 40;

/// One cached window snapshot: bottom-up 32bpp BGRX rows (bottom-up so
/// `StretchDIBits` consumes it without source-rect flipping quirks).
pub struct Snapshot {
    pub width: i32,
    pub height: i32,
    pub bits: Vec<u8>,
}

/// LRU-ish snapshot store: entries are stamped at insert; eviction drops
/// the smallest stamp. Plain data, unit-testable without GDI.
struct SnapshotCache {
    entries: HashMap<u64, (u64, Arc<Snapshot>)>,
    next_stamp: u64,
}

impl SnapshotCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_stamp: 0,
        }
    }

    fn insert(&mut self, wid: u64, snap: Snapshot) {
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        self.entries.insert(wid, (stamp, Arc::new(snap)));
        while self.entries.len() > MAX_ENTRIES {
            let Some(&oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (stamp, _))| *stamp)
                .map(|(wid, _)| wid)
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn get(&self, wid: u64) -> Option<Arc<Snapshot>> {
        self.entries.get(&wid).map(|(_, snap)| Arc::clone(snap))
    }

    fn remove(&mut self, wid: u64) {
        self.entries.remove(&wid);
    }
}

static CACHE: LazyLock<Mutex<SnapshotCache>> = LazyLock::new(|| Mutex::new(SnapshotCache::new()));

fn cache() -> MutexGuard<'static, SnapshotCache> {
    CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Capture `wid` into the snapshot cache (replacing any previous entry).
/// Returns false when the window is gone or the capture fails; the old
/// cached snapshot (if any) is kept in that case.
pub fn snapshot_capture(wid: WindowId) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(wid) else {
        return false;
    };
    let Some(snap) = (unsafe { capture_window(hwnd) }) else {
        return false;
    };
    cache().insert(wid, snap);
    true
}

/// Fetch the cached snapshot for `wid`, if any.
pub fn snapshot_get(wid: WindowId) -> Option<Arc<Snapshot>> {
    cache().get(wid)
}

/// Drop the cached snapshot for `wid` (window destroyed / unmanaged).
pub fn snapshot_remove(wid: WindowId) {
    cache().remove(wid);
}

/// A 32bpp bottom-up DIB section selected into a memory DC.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut core::ffi::c_void,
}

unsafe fn make_dib(screen_dc: HDC, w: i32, h: i32) -> Option<Dib> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: h, // positive = bottom-up
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(screen_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let dc = CreateCompatibleDC(Some(screen_dc));
    let old = SelectObject(dc, bmp.into());
    Some(Dib { dc, bmp, old, bits })
}

unsafe fn release_dib(dib: Dib) {
    SelectObject(dib.dc, dib.old);
    let _ = DeleteDC(dib.dc);
    let _ = DeleteObject(dib.bmp.into());
}

/// Downscaled dimensions capping the longest side at [`MAX_SIDE`].
fn scaled_dims(w: i32, h: i32) -> (i32, i32) {
    let longest = w.max(h);
    if longest <= MAX_SIDE {
        return (w, h);
    }
    let scale = f64::from(MAX_SIDE) / f64::from(longest);
    (
        ((f64::from(w) * scale).round() as i32).max(1),
        ((f64::from(h) * scale).round() as i32).max(1),
    )
}

/// PrintWindow the full composed window content into a full-size DIB,
/// then HALFTONE-StretchBlt it down to the cache size.
unsafe fn capture_window(hwnd: windows::Win32::Foundation::HWND) -> Option<Snapshot> {
    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).ok()?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 {
        return None;
    }

    let screen_dc = GetDC(None);
    let result = capture_into_dibs(screen_dc, hwnd, w, h);
    ReleaseDC(None, screen_dc);
    result
}

unsafe fn capture_into_dibs(
    screen_dc: HDC,
    hwnd: windows::Win32::Foundation::HWND,
    w: i32,
    h: i32,
) -> Option<Snapshot> {
    let full = make_dib(screen_dc, w, h)?;
    if !PrintWindow(hwnd, full.dc, PW_RENDERFULLCONTENT).as_bool() {
        release_dib(full);
        return None;
    }
    let (dw, dh) = scaled_dims(w, h);
    let snap = if (dw, dh) == (w, h) {
        let _ = GdiFlush();
        copy_bits(&full, w, h)
    } else {
        let scaled = match make_dib(screen_dc, dw, dh) {
            Some(d) => d,
            None => {
                release_dib(full);
                return None;
            }
        };
        SetStretchBltMode(scaled.dc, HALFTONE);
        let _ = SetBrushOrgEx(scaled.dc, 0, 0, None);
        let blit = StretchBlt(scaled.dc, 0, 0, dw, dh, Some(full.dc), 0, 0, w, h, SRCCOPY);
        let _ = GdiFlush();
        let snap = blit.as_bool().then(|| copy_bits(&scaled, dw, dh)).flatten();
        release_dib(scaled);
        snap
    };
    release_dib(full);
    snap
}

unsafe fn copy_bits(dib: &Dib, w: i32, h: i32) -> Option<Snapshot> {
    if dib.bits.is_null() {
        return None;
    }
    let len = (w as usize) * (h as usize) * 4;
    let src = std::slice::from_raw_parts(dib.bits as *const u8, len);
    Some(Snapshot {
        width: w,
        height: h,
        bits: src.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy(w: i32, h: i32) -> Snapshot {
        Snapshot {
            width: w,
            height: h,
            bits: vec![0; (w * h * 4) as usize],
        }
    }

    #[test]
    fn test_cache_evicts_oldest_beyond_capacity() {
        let mut cache = SnapshotCache::new();
        for wid in 0..(MAX_ENTRIES as u64 + 5) {
            cache.insert(wid, dummy(4, 4));
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        for wid in 0..5 {
            assert!(cache.get(wid).is_none(), "oldest entry {wid} must be evicted");
        }
        assert!(cache.get(MAX_ENTRIES as u64 + 4).is_some(), "newest entry stays");
    }

    #[test]
    fn test_cache_reinsert_refreshes_stamp() {
        let mut cache = SnapshotCache::new();
        for wid in 0..MAX_ENTRIES as u64 {
            cache.insert(wid, dummy(4, 4));
        }
        // Refresh wid 0, then overflow by one: wid 1 (now oldest) goes.
        cache.insert(0, dummy(8, 8));
        cache.insert(999, dummy(4, 4));
        assert!(cache.get(0).is_some(), "refreshed entry survives");
        assert!(cache.get(1).is_none(), "stale entry evicted");
        assert_eq!(cache.get(0).unwrap().width, 8, "refresh replaced the snapshot");
    }

    #[test]
    fn test_cache_remove() {
        let mut cache = SnapshotCache::new();
        cache.insert(7, dummy(4, 4));
        cache.remove(7);
        assert!(cache.get(7).is_none());
    }

    #[test]
    fn test_scaled_dims_caps_longest_side() {
        assert_eq!(scaled_dims(1920, 1080), (480, 270));
        assert_eq!(scaled_dims(1080, 1920), (270, 480));
        assert_eq!(scaled_dims(400, 300), (400, 300), "small windows keep their size");
        assert_eq!(scaled_dims(5000, 10), (480, 1), "extreme aspect clamps to 1px");
    }

    #[test]
    fn test_capture_invalid_window_returns_false() {
        assert!(!snapshot_capture(0xDEAD_BEEF));
        assert!(snapshot_get(0xDEAD_BEEF).is_none());
    }
}
