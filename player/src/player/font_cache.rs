//! Subtitle fonts the manifest lists, fetched in parallel and kept on disk.
//!
//! A styled (ASS) track names its fonts in the MPD (DVB font download), and
//! every episode of a series names the same ones. Without a cache each track
//! selection downloaded them again, one after another, while the subtitles
//! waited. With a cache directory set ([`Player::set_font_cache_dir`]) a font
//! is read from disk when it is there and written there after a download.
//!
//! The player only reads and writes files in that directory; how big it may
//! grow and when it is cleaned is the host's call. To make that easy, a hit
//! refreshes the file's modification time, so "delete the oldest by mtime"
//! is a least-recently-used policy. Nothing is held in memory past the track
//! that uses it.
//!
//! The cache key is the font URL without its query string and fragment:
//! signed or tokenised URLs change their query on every manifest, while the
//! file behind the path stays the same.

use super::*;
use crate::BoxError;

/// Fonts fetched at once (the rest wait their turn).
const FONT_FETCH_CONCURRENCY: usize = 4;

/// Fetch every font in `fonts`, from the cache when it has it. Results are in
/// `fonts` order.
pub(super) async fn fetch_fonts(
    fonts: &[crate::manifest::FontDownload],
    http: &HttpClient,
    cache_dir: Option<std::path::PathBuf>,
) -> Vec<Result<Vec<u8>, BoxError>> {
    use futures_util::stream::{self, StreamExt};
    // The futures are built up front (not by a closure inside the stream):
    // a borrowing closure in a stream makes the spawned task's future fail
    // the `Send` check over the closure's lifetimes.
    let jobs: Vec<_> = fonts.iter().map(|font| fetch_font(&font.url, http, cache_dir.clone())).collect();
    stream::iter(jobs).buffered(FONT_FETCH_CONCURRENCY).collect().await
}

async fn fetch_font(url: &str, http: &HttpClient, cache_dir: Option<std::path::PathBuf>) -> Result<Vec<u8>, BoxError> {
    if let Some(dir) = &cache_dir {
        if let Some(bytes) = disk::load(dir, url).await {
            log::info!("[subs] font {} from cache ({} kB)", url, bytes.len() / 1024);
            return Ok(bytes);
        }
    }
    let bytes = http.get(url.to_string(), RequestKind::InitSegment).await?.to_vec();
    if let Some(dir) = cache_dir {
        disk::store(dir, url, bytes.clone()).await;
    }
    Ok(bytes)
}

/// The cache file name for `url`: SHA-256 of the URL without query and
/// fragment, hex.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn cache_key(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    #[cfg(not(target_arch = "wasm32"))]
    {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(path.as_bytes()))
    }
    #[cfg(target_arch = "wasm32")]
    {
        // No disk cache in the browser (its HTTP cache covers it); only here
        // so the function exists.
        path.to_string()
    }
}

/// Whether `bytes` start like a font file (TrueType, OpenType, a collection,
/// WOFF / WOFF2). A file cut short or overwritten by something else is
/// fetched again instead of handed to the renderer.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn looks_like_font(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some([0x00, 0x01, 0x00, 0x00]) | Some(b"OTTO") | Some(b"true") | Some(b"ttcf") | Some(b"wOFF") | Some(b"wOF2")
    )
}

#[cfg(not(target_arch = "wasm32"))]
mod disk {
    use std::path::{Path, PathBuf};

    fn path_for(dir: &Path, url: &str) -> PathBuf {
        dir.join(format!("{}.font", super::cache_key(url)))
    }

    /// The cached font for `url`, refreshing its modification time. `None`
    /// when it is not there or not a font (a bad file is removed).
    pub(super) async fn load(dir: &Path, url: &str) -> Option<Vec<u8>> {
        let path = path_for(dir, url);
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).ok()?;
            if !super::looks_like_font(&bytes) {
                log::warn!("[subs] cached font {} is not a font, fetching again", path.display());
                let _ = std::fs::remove_file(&path);
                return None;
            }
            if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                let _ = file.set_modified(std::time::SystemTime::now());
            }
            Some(bytes)
        })
        .await
        .ok()
        .flatten()
    }

    /// Write `bytes` as the cached font for `url`. Written to a temporary
    /// file and renamed, so a reader never sees half a font. Failures are
    /// logged and otherwise ignored: the font is already in hand.
    pub(super) async fn store(dir: PathBuf, url: &str, bytes: Vec<u8>) {
        if !super::looks_like_font(&bytes) {
            log::info!("[subs] font {} is not a known font format, not caching it", url);
            return;
        }
        let path = path_for(&dir, url);
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("[subs] font cache write failed: {}", e),
            Err(e) => log::warn!("[subs] font cache write task failed: {}", e),
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod disk {
    use std::path::{Path, PathBuf};
    pub(super) async fn load(_dir: &Path, _url: &str) -> Option<Vec<u8>> {
        None
    }
    pub(super) async fn store(_dir: PathBuf, _url: &str, _bytes: Vec<u8>) {}
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    const TTF: &[u8] = &[0x00, 0x01, 0x00, 0x00, 1, 2, 3, 4];

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rp-font-cache-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn key_ignores_query_and_fragment() {
        assert_eq!(cache_key("https://cdn/a/Font.ttf?sig=1"), cache_key("https://cdn/a/Font.ttf?sig=2#x"));
        assert_ne!(cache_key("https://cdn/a/Font.ttf"), cache_key("https://cdn/b/Font.ttf"));
    }

    #[test]
    fn font_signatures() {
        assert!(looks_like_font(TTF));
        assert!(looks_like_font(b"OTTO...."));
        assert!(looks_like_font(b"wOF2...."));
        assert!(!looks_like_font(b"<html>"));
        assert!(!looks_like_font(b""));
    }

    #[tokio::test]
    async fn store_then_load_round_trips() {
        let dir = temp_dir("rt");
        let url = "https://cdn/fonts/A.ttf?token=abc";
        assert!(disk::load(&dir, url).await.is_none());
        disk::store(dir.clone(), url, TTF.to_vec()).await;
        assert_eq!(disk::load(&dir, "https://cdn/fonts/A.ttf?token=other").await.as_deref(), Some(TTF));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_fonts_are_not_cached_and_bad_files_are_dropped() {
        let dir = temp_dir("bad");
        let url = "https://cdn/fonts/B.ttf";
        disk::store(dir.clone(), url, b"<html>error</html>".to_vec()).await;
        assert!(disk::load(&dir, url).await.is_none());

        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.font", cache_key(url)));
        std::fs::write(&path, b"garbage").unwrap();
        assert!(disk::load(&dir, url).await.is_none());
        assert!(!path.exists(), "a bad cache file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
