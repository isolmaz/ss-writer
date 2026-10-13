//! The weekly update check (spec §13): ask GitHub for the latest release, compare it with the
//! running version and report it; switched off by the `check_for_updates` setting. The time of
//! the last successful check is kept in the state database so the check runs at most once a
//! week however often the app starts. The check itself only notifies; the installer of a
//! reported release is downloaded, and verified against the release's `SHA256SUMS.txt`, by
//! [`download_installer`] when the user asks for it.

use std::cmp::Ordering;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::net::{Accept, FetchOptions, Http, NetError, Scheme, Url, download, fetch};
use crate::store::{Store, StoreError, UnixSeconds};

/// The release feed of the project.
pub const RELEASES_URL: &str = "https://api.github.com/repos/isolmaz/ss-writer/releases/latest";
/// Time between two checks.
pub const CHECK_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;
/// Largest answer read from the feed.
pub const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// Deadline of one check.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest installer accepted.
pub const MAX_INSTALLER_BYTES: u64 = 200 * 1024 * 1024;
/// Largest `SHA256SUMS.txt` accepted: a few lines of 64 digits and file names.
pub const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;
/// Deadline of an installer download, redirects included. The 15 seconds of a check are too
/// short for a 35 MB file on a slow line; the checksum list is still fetched within
/// [`CHECK_TIMEOUT`].
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const LAST_CHECK_KEY: &str = "update.last_check";
/// The checksum list every release carries: `<64 hex digits>  <file name>` per line.
const CHECKSUMS_NAME: &str = "SHA256SUMS.txt";

/// A text that is not a semantic version (`1.2.3`, optionally `-pre.release` and `+build`,
/// optionally with a leading `v`).
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("`{0}` is not a semantic version")]
pub struct VersionError(String);

/// One dot-separated identifier of a pre-release suffix. Numeric identifiers sort before
/// alphanumeric ones (the derived order follows the variant order).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    Numeric(u64),
    Text(String),
}

/// A semantic version (build metadata is dropped: it never takes part in precedence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<PreId>,
}

fn numeric(part: &str) -> Option<u64> {
    let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let canonical = part == "0" || !part.starts_with('0');
    if digits && canonical {
        part.parse().ok()
    } else {
        None
    }
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2.3-rc.1`, `1.2.3+build`.
    ///
    /// # Errors
    /// [`VersionError`] when `text` is not a semantic version.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let fail = || VersionError(text.to_owned());
        let trimmed = text.trim();
        let trimmed = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
        let without_build = trimmed.split('+').next().unwrap_or(trimmed);
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let mut numbers = core.split('.').map(numeric);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) = (
            numbers.next(),
            numbers.next(),
            numbers.next(),
            numbers.next(),
        ) else {
            return Err(fail());
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|id| {
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        None
                    } else if id.bytes().all(|b| b.is_ascii_digit()) {
                        numeric(id).map(PreId::Numeric)
                    } else {
                        Some(PreId::Text(id.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(fail)?,
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// True for `1.2.3-rc.1`-style versions.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release is newer than its own pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, id) in self.pre.iter().enumerate() {
            f.write_str(if index == 0 { "-" } else { "." })?;
            match id {
                PreId::Numeric(number) => write!(f, "{number}")?,
                PreId::Text(text) => f.write_str(text)?,
            }
        }
        Ok(())
    }
}

/// A file attached to a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// The file name (`SSWriter-1.2.0-x64.msi`).
    pub name: String,
    /// The `https` URL the file is downloaded from.
    pub url: String,
    /// The size in bytes as published.
    pub size: u64,
}

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The tag as published (`v1.2.0`).
    pub tag: String,
    /// The `https` page describing the release.
    pub page_url: String,
    /// The files attached to the release, in the order the feed lists them.
    pub assets: Vec<Asset>,
}

/// Outcome of [`check_for_updates`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Update checks are switched off.
    Disabled,
    /// The last check is less than a week old; nothing was requested.
    NotDue,
    /// The latest release is not newer than the running version.
    UpToDate,
    /// A newer release exists.
    Available(Release),
}

/// Why the check or the download of an installer failed (a check is retried at the next start).
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the release feed answered with invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Version(#[from] VersionError),
    #[error("the release page URL is not an https URL")]
    InsecurePage,
    #[error("the download URL of `{0}` is not an https URL")]
    InsecureAsset(String),
    #[error("the release has no `{0}` to download")]
    MissingAsset(String),
    #[error("{CHECKSUMS_NAME} does not list `{0}`")]
    MissingChecksum(String),
    #[error(
        "the downloaded `{name}` does not match its checksum (expected {expected}, got {actual})"
    )]
    ChecksumMismatch {
        name: String,
        /// The digest `SHA256SUMS.txt` lists, lower-case hex.
        expected: String,
        /// The digest of the downloaded file, lower-case hex.
        actual: String,
    },
    #[error("i/o error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Whether a check is due: none was made, or the last one is a week or more old (a clock
/// that moved backwards also makes it due).
#[must_use]
pub fn is_due(last_check: Option<UnixSeconds>, now: UnixSeconds) -> bool {
    last_check.is_none_or(|last| !(last..last.saturating_add(CHECK_INTERVAL_SECS)).contains(&now))
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

fn is_https(url: &str) -> bool {
    Url::parse(url).map(|url| url.scheme()) == Ok(Scheme::Https)
}

/// The release described by the feed's JSON answer.
///
/// # Errors
/// [`UpdateError::Json`], [`UpdateError::Version`] for an unparsable tag,
/// [`UpdateError::InsecurePage`] when the page URL is not `https` and
/// [`UpdateError::InsecureAsset`] when the download URL of an asset is not.
pub fn parse_release(body: &[u8]) -> Result<Release, UpdateError> {
    let json: ReleaseJson = serde_json::from_slice(body)?;
    let version = Version::parse(&json.tag_name)?;
    if !is_https(&json.html_url) {
        return Err(UpdateError::InsecurePage);
    }
    let assets = json
        .assets
        .into_iter()
        .map(|asset| {
            if is_https(&asset.browser_download_url) {
                Ok(Asset {
                    name: asset.name,
                    url: asset.browser_download_url,
                    size: asset.size,
                })
            } else {
                Err(UpdateError::InsecureAsset(asset.name))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Release {
        version,
        tag: json.tag_name,
        page_url: json.html_url,
        assets,
    })
}

/// The per-user MSI of `release` (`SSWriter-<version>-x64.msi`).
#[must_use]
pub fn installer_asset(release: &Release) -> Option<&Asset> {
    let name = installer_name(&release.version);
    release.assets.iter().find(|asset| asset.name == name)
}

/// The checksum list of `release` (`SHA256SUMS.txt`).
#[must_use]
pub fn checksums_asset(release: &Release) -> Option<&Asset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == CHECKSUMS_NAME)
}

fn installer_name(version: &Version) -> String {
    format!("SSWriter-{version}-x64.msi")
}

/// The digest `sums` (the text of `SHA256SUMS.txt`: `<64 hex>  <name>` per line, a `*` before
/// the name marking binary mode as `sha256sum` writes it) records for the file `name`,
/// lower-cased. Lines that are not of that shape and other names are ignored; when a name is
/// listed twice the first line counts.
#[must_use]
pub fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (digest, listed) = line.trim().split_once(char::is_whitespace)?;
        let listed = listed.trim_start();
        let listed = listed.strip_prefix('*').unwrap_or(listed);
        let valid = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
        (valid && listed == name).then(|| digest.to_ascii_lowercase())
    })
}

/// The latest release published at `endpoint` (normally [`RELEASES_URL`]), asked for now: no
/// schedule, no setting and no record of the check (the "Check for updates now" command).
///
/// # Errors
/// [`UpdateError::Net`] for network and protocol failures and the errors of [`parse_release`].
pub fn latest_release(http: &dyn Http, endpoint: &str) -> Result<Release, UpdateError> {
    let fetched = fetch(
        http,
        endpoint,
        &FetchOptions {
            accept: Accept::Json,
            max_body: MAX_RESPONSE_BYTES,
            timeout: CHECK_TIMEOUT,
            allow_http: false,
        },
    )?;
    parse_release(&fetched.body)
}

/// Download the installer of `release` ([`installer_asset`]) into `dir` (created when missing)
/// as a file of the asset's name, reporting `(downloaded, total)` to `progress` after every
/// chunk, and verify it against the release's `SHA256SUMS.txt` ([`checksums_asset`]). The list
/// is fetched first, so a release that does not cover its installer fails before the big
/// download starts. Returns the path of the verified file; a file that does not match its
/// checksum is deleted.
///
/// # Errors
/// [`UpdateError::MissingAsset`] when the release has no installer or no checksum list,
/// [`UpdateError::MissingChecksum`] when the list does not name the installer,
/// [`UpdateError::ChecksumMismatch`] when the download does not match, [`UpdateError::Io`]
/// when `dir` cannot be created and [`UpdateError::Net`] for network, protocol and file
/// failures of either download, including a body over [`MAX_INSTALLER_BYTES`] (installer) or
/// [`MAX_CHECKSUMS_BYTES`] (list) and a deadline of [`DOWNLOAD_TIMEOUT`] (installer) or
/// [`CHECK_TIMEOUT`] (list).
pub fn download_installer(
    http: &dyn Http,
    release: &Release,
    dir: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, UpdateError> {
    let installer = installer_asset(release)
        .ok_or_else(|| UpdateError::MissingAsset(installer_name(&release.version)))?;
    let checksums = checksums_asset(release)
        .ok_or_else(|| UpdateError::MissingAsset(CHECKSUMS_NAME.to_owned()))?;
    let list = fetch(
        http,
        &checksums.url,
        &FetchOptions {
            accept: Accept::Any,
            max_body: MAX_CHECKSUMS_BYTES,
            timeout: CHECK_TIMEOUT,
            allow_http: false,
        },
    )?;
    let expected = expected_sha256(&String::from_utf8_lossy(&list.body), &installer.name)
        .ok_or_else(|| UpdateError::MissingChecksum(installer.name.clone()))?;
    std::fs::create_dir_all(dir).map_err(|source| UpdateError::Io {
        path: dir.to_owned(),
        source,
    })?;
    // The asset name is `SSWriter-<version>-x64.msi` with a parsed version: no separators.
    let path = dir.join(&installer.name);
    let downloaded = download(
        http,
        &installer.url,
        &FetchOptions {
            accept: Accept::Any,
            max_body: MAX_INSTALLER_BYTES,
            timeout: DOWNLOAD_TIMEOUT,
            allow_http: false,
        },
        &path,
        progress,
    )?;
    if downloaded.sha256 != expected {
        // Best effort: the mismatch is the error worth reporting.
        let _ = std::fs::remove_file(&path);
        return Err(UpdateError::ChecksumMismatch {
            name: installer.name.clone(),
            expected,
            actual: downloaded.sha256,
        });
    }
    Ok(path)
}

/// The time of the last successful check, if any.
///
/// # Errors
/// [`StoreError::Sqlite`] on database failure.
pub fn last_check(store: &Store) -> Result<Option<UnixSeconds>, StoreError> {
    Ok(store
        .kv_get(LAST_CHECK_KEY)?
        .and_then(|text| text.parse().ok()))
}

/// Check `endpoint` (normally [`RELEASES_URL`]) for a release newer than `current`, unless
/// `enabled` is false or the last successful check is less than [`CHECK_INTERVAL_SECS`] old.
/// A successful answer is recorded as the last check, newer release or not; a failure leaves
/// the record alone so the next start tries again.
///
/// # Errors
/// [`UpdateError`] for network, protocol, JSON, version and database failures.
pub fn check_for_updates(
    store: &Store,
    http: &dyn Http,
    endpoint: &str,
    current: &Version,
    enabled: bool,
    now: UnixSeconds,
) -> Result<UpdateStatus, UpdateError> {
    if !enabled {
        return Ok(UpdateStatus::Disabled);
    }
    if !is_due(last_check(store)?, now) {
        return Ok(UpdateStatus::NotDue);
    }
    let release = latest_release(http, endpoint)?;
    store.kv_set(LAST_CHECK_KEY, &now.to_string())?;
    Ok(if release.version > *current {
        UpdateStatus::Available(release)
    } else {
        UpdateStatus::UpToDate
    })
}
