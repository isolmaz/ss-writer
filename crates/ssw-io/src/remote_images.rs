//! Remote images of Markdown documents (spec §3.3): `https` only, image content types only,
//! 20 MB per image, 15 s deadline, no cookies; downloads are cached on disk (500 MB, least
//! recently used first out) with their metadata in the `image_cache` table of the state
//! database.
//!
//! `http` URLs are [`ImageLoad::Blocked`] (the UI shows a "Click to load" placeholder) unless the
//! caller passes `allow_http` after the user asked for it.
//!
//! [`RemoteImages::load`] is the whole flow. Callers that share the [`Store`] between threads
//! call the three steps themselves so the store is not held during the download:
//! [`RemoteImages::lookup`], [`RemoteImages::download`], [`RemoteImages::insert`].

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use thiserror::Error;

use crate::net::{Accept, FetchOptions, Http, NetError, Scheme, Url, fetch};
use crate::store::{Store, StoreError, UnixSeconds};

/// Largest image downloaded.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Deadline of one download.
pub const IMAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// Default size of the disk cache.
pub const CACHE_LIMIT_BYTES: u64 = 500 * 1024 * 1024;

/// Why an image could not be provided.
#[derive(Debug, Error)]
pub enum ImageError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("i/o error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A downloaded image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteImage {
    /// Content type without parameters, lower-case (`image/png`).
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Served from the disk cache rather than the network.
    pub from_cache: bool,
}

/// Result of [`RemoteImages::download`] and [`RemoteImages::load`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageLoad {
    Loaded(RemoteImage),
    /// The URL is `http` and was not explicitly allowed.
    Blocked,
}

/// The remote image cache living in one folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteImages {
    dir: PathBuf,
    limit: u64,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ImageError + '_ {
    move |source| ImageError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Whether `url` is plain `http` that the user has not allowed.
fn is_blocked(url: &str, allow_http: bool) -> Result<bool, NetError> {
    Ok(Url::parse(url)?.scheme() == Scheme::Http && !allow_http)
}

/// Download the image at `url` under the image limits.
fn fetch_image(http: &dyn Http, url: &str, allow_http: bool) -> Result<RemoteImage, NetError> {
    let fetched = fetch(
        http,
        url,
        &FetchOptions {
            accept: Accept::Image,
            max_body: MAX_IMAGE_BYTES,
            timeout: IMAGE_TIMEOUT,
            allow_http,
        },
    )?;
    Ok(RemoteImage {
        // `fetch` only succeeds when the content type is `image/*`, so it is present.
        mime: fetched.mime.unwrap_or_default(),
        bytes: fetched.body,
        from_cache: false,
    })
}

/// The cache key of an (already normalized) URL.
fn key_of_url(url: &Url) -> String {
    blake3::hash(url.to_string().as_bytes())
        .to_hex()
        .to_string()
}

impl RemoteImages {
    /// A cache in `dir` (created on first write) limited to [`CACHE_LIMIT_BYTES`].
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limit(dir, CACHE_LIMIT_BYTES)
    }

    /// A cache in `dir` limited to `limit` bytes.
    #[must_use]
    pub fn with_limit(dir: PathBuf, limit: u64) -> Self {
        Self { dir, limit }
    }

    /// The cached image for `url`, marking it as just used. A cache entry whose file is gone
    /// or has the wrong size is dropped and reported as a miss.
    ///
    /// # Errors
    /// [`ImageError::Net`] for an invalid URL and [`ImageError::Store`] on database failure.
    pub fn lookup(
        &self,
        store: &Store,
        url: &str,
        now: UnixSeconds,
    ) -> Result<Option<RemoteImage>, ImageError> {
        let key = key_of_url(&Url::parse(url)?);
        let Some((mime, size)) = store
            .conn
            .query_row(
                "SELECT mime, size FROM image_cache WHERE key = ?1",
                [&key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?
        else {
            return Ok(None);
        };
        match std::fs::read(self.dir.join(&key)) {
            Ok(bytes) if bytes.len() as i64 == size => {
                store
                    .conn
                    .execute(
                        "UPDATE image_cache SET last_access = ?2 WHERE key = ?1",
                        params![key, now],
                    )
                    .map_err(StoreError::from)?;
                Ok(Some(RemoteImage {
                    mime,
                    bytes,
                    from_cache: true,
                }))
            }
            _ => {
                store
                    .conn
                    .execute("DELETE FROM image_cache WHERE key = ?1", [&key])
                    .map_err(StoreError::from)?;
                Ok(None)
            }
        }
    }

    /// Download `url` (no cache, no store).
    ///
    /// # Errors
    /// [`ImageError::Net`]: invalid URL, redirects to `http`, non-200 status, content type that
    /// is not `image/*`, more than [`MAX_IMAGE_BYTES`], timeout, transport failure.
    pub fn download(
        &self,
        http: &dyn Http,
        url: &str,
        allow_http: bool,
    ) -> Result<ImageLoad, ImageError> {
        if is_blocked(url, allow_http)? {
            return Ok(ImageLoad::Blocked);
        }
        Ok(ImageLoad::Loaded(fetch_image(http, url, allow_http)?))
    }

    /// Put `image` (downloaded from `url`) into the cache and evict the least recently used
    /// entries until the cache fits its limit again. The new entry itself is never evicted.
    ///
    /// # Errors
    /// [`ImageError::Net`] for an invalid URL, [`ImageError::Io`] when a cache file cannot be
    /// written or removed and [`ImageError::Store`] on database failure.
    pub fn insert(
        &self,
        store: &Store,
        url: &str,
        image: &RemoteImage,
        now: UnixSeconds,
    ) -> Result<(), ImageError> {
        let parsed = Url::parse(url)?;
        let key = key_of_url(&parsed);
        std::fs::create_dir_all(&self.dir).map_err(io_error(&self.dir))?;
        let temp = self.dir.join(format!("{key}.tmp"));
        let target = self.dir.join(&key);
        std::fs::write(&temp, &image.bytes).map_err(io_error(&temp))?;
        if let Err(source) = std::fs::rename(&temp, &target) {
            // Best effort: the rename error is the one worth reporting.
            let _ = std::fs::remove_file(&temp);
            return Err(ImageError::Io {
                path: target,
                source,
            });
        }
        store
            .conn
            .execute(
                "INSERT INTO image_cache (key, url, mime, size, last_access)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key) DO UPDATE SET
                     url = excluded.url, mime = excluded.mime,
                     size = excluded.size, last_access = excluded.last_access",
                params![
                    key,
                    parsed.to_string(),
                    image.mime,
                    image.bytes.len() as i64,
                    now
                ],
            )
            .map_err(StoreError::from)?;
        self.evict(store, &key)
    }

    /// Delete the least recently used entries (never `keep`) until the cache fits the limit.
    fn evict(&self, store: &Store, keep: &str) -> Result<(), ImageError> {
        let mut total: i64 = store
            .conn
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM image_cache",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        let limit = i64::try_from(self.limit).unwrap_or(i64::MAX);
        if total <= limit {
            return Ok(());
        }
        let mut stmt = store
            .conn
            .prepare("SELECT key, size FROM image_cache WHERE key != ?1 ORDER BY last_access, key")
            .map_err(StoreError::from)?;
        let victims = stmt
            .query_map([keep], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(StoreError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        for (key, size) in victims {
            if total <= limit {
                break;
            }
            let file = self.dir.join(&key);
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(ImageError::Io { path: file, source }),
            }
            store
                .conn
                .execute("DELETE FROM image_cache WHERE key = ?1", [&key])
                .map_err(StoreError::from)?;
            total -= size;
        }
        Ok(())
    }

    /// The image at `url`: from the cache, or downloaded and cached.
    ///
    /// # Errors
    /// See [`RemoteImages::lookup`], [`RemoteImages::download`] and [`RemoteImages::insert`].
    pub fn load(
        &self,
        store: &Store,
        http: &dyn Http,
        url: &str,
        allow_http: bool,
        now: UnixSeconds,
    ) -> Result<ImageLoad, ImageError> {
        if is_blocked(url, allow_http)? {
            return Ok(ImageLoad::Blocked);
        }
        if let Some(image) = self.lookup(store, url, now)? {
            return Ok(ImageLoad::Loaded(image));
        }
        let image = fetch_image(http, url, allow_http)?;
        self.insert(store, url, &image, now)?;
        Ok(ImageLoad::Loaded(image))
    }
}
