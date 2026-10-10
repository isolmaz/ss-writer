//! The weekly update check (spec §13): ask GitHub for the latest release, compare it with the
//! running version and report it; notification only, never a download, and switched off by the
//! `check_for_updates` setting. The time of the last successful check is kept in the state
//! database so the check runs at most once a week however often the app starts.

use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::net::{Accept, FetchOptions, Http, NetError, Scheme, Url, fetch};
use crate::store::{Store, StoreError, UnixSeconds};

/// The release feed of the project.
pub const RELEASES_URL: &str = "https://api.github.com/repos/isolmaz/ss-writer/releases/latest";
/// Time between two checks.
pub const CHECK_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;
/// Largest answer read from the feed.
pub const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// Deadline of one check.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

const LAST_CHECK_KEY: &str = "update.last_check";

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

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The tag as published (`v1.2.0`).
    pub tag: String,
    /// The `https` page describing the release.
    pub page_url: String,
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

/// Why the check failed (it is retried at the next start).
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
}

/// Whether a check is due: none was made, or the last one is a week or more old (a clock
/// that moved backwards also makes it due).
#[must_use]
pub fn is_due(last_check: Option<UnixSeconds>, now: UnixSeconds) -> bool {
    last_check.is_none_or(|last| !(last..last.saturating_add(CHECK_INTERVAL_SECS)).contains(&now))
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    html_url: String,
}

/// The release described by the feed's JSON answer.
///
/// # Errors
/// [`UpdateError::Json`], [`UpdateError::Version`] for an unparsable tag and
/// [`UpdateError::InsecurePage`] when the page URL is not `https`.
pub fn parse_release(body: &[u8]) -> Result<Release, UpdateError> {
    let json: ReleaseJson = serde_json::from_slice(body)?;
    let version = Version::parse(&json.tag_name)?;
    if Url::parse(&json.html_url).map(|url| url.scheme()) != Ok(Scheme::Https) {
        return Err(UpdateError::InsecurePage);
    }
    Ok(Release {
        version,
        tag: json.tag_name,
        page_url: json.html_url,
    })
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
    let release = parse_release(&fetched.body)?;
    store.kv_set(LAST_CHECK_KEY, &now.to_string())?;
    Ok(if release.version > *current {
        UpdateStatus::Available(release)
    } else {
        UpdateStatus::UpToDate
    })
}
