//! Remote images of Markdown documents (spec §3.3): which images a document may load and the
//! fetch itself.
//!
//! `https` images load by themselves (Settings → Attachments → "Load remote images") unless the
//! document opted out with the banner's "Don't load remote images". `http` images, and the
//! images of an opted-out document, show a "Click to load" placeholder; the click allows that
//! URL for the document ([`RemoteImagePolicy::allow_url`]). The decisions are kept per open
//! document ([`DocId`]) in the [`RemoteImagePolicy`] the controller owns; the window's object
//! provider ([`super::platform`]) asks it and bumps nothing itself.
//!
//! [`fetch_remote_image`] is the worker side: the disk cache first, then the download under the
//! limits of `ssw_io::remote_images`, on a connection to the state database of its own (the
//! UI thread's connection is not shared with workers).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use ssw_io::net::Http;
use ssw_io::recovery::now_millis;
use ssw_io::remote_images::{ImageLoad, RemoteImages};
use ssw_io::store::Store;

use super::controller::{Controller, WindowCell};
use crate::documents::DocId;

/// Whether `destination` points at the network (`http` or `https`, any case).
#[must_use]
pub fn is_remote(destination: &str) -> bool {
    starts_with_scheme(destination, "http://") || is_https(destination)
}

/// Whether `destination` is an `https` URL.
#[must_use]
pub fn is_https(destination: &str) -> bool {
    starts_with_scheme(destination, "https://")
}

fn starts_with_scheme(destination: &str, scheme: &str) -> bool {
    destination
        .get(..scheme.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
}

/// What each open document may load from the network.
#[derive(Debug, Default)]
pub struct RemoteImagePolicy {
    /// Documents whose "Don't load remote images" was chosen.
    disabled: RefCell<HashSet<DocId>>,
    /// The URLs the user asked to load ("Click to load"), per document.
    allowed: RefCell<HashSet<(DocId, String)>>,
    /// Counts the changes, so the window knows when to lay the images out again.
    epoch: Cell<u64>,
}

impl RemoteImagePolicy {
    /// Whether `doc` opted out of loading remote images by itself.
    #[must_use]
    pub fn is_disabled(&self, doc: DocId) -> bool {
        self.disabled.borrow().contains(&doc)
    }

    /// Opts `doc` out of (`true`) or back into (`false`) loading remote images by itself.
    pub fn set_disabled(&self, doc: DocId, disabled: bool) {
        let changed = if disabled {
            self.disabled.borrow_mut().insert(doc)
        } else {
            self.disabled.borrow_mut().remove(&doc)
        };
        if changed {
            self.bump();
        }
    }

    /// Whether the user asked `doc` to load `url`.
    #[must_use]
    pub fn url_allowed(&self, doc: DocId, url: &str) -> bool {
        self.allowed
            .borrow()
            .iter()
            .any(|(allowed_doc, allowed_url)| *allowed_doc == doc && allowed_url == url)
    }

    /// The user asked `doc` to load `url` (an `http` image, or any image of an opted-out
    /// document).
    pub fn allow_url(&self, doc: DocId, url: &str) {
        if self.allowed.borrow_mut().insert((doc, url.to_owned())) {
            self.bump();
        }
    }

    /// Changes so far: differs from an earlier value when something was allowed or disabled
    /// meanwhile.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    fn bump(&self) {
        self.epoch.set(self.epoch.get() + 1);
    }
}

impl Controller {
    /// What the documents may load from the network.
    #[must_use]
    pub fn remote_images(&self) -> &Rc<RemoteImagePolicy> {
        &self.remote_images
    }

    /// The remote image policy of the active document of `cell` changed (the banner's "Don't
    /// load remote images"): the window lays its images out again and repaints.
    pub fn remote_images_changed(&self, cell: &Rc<WindowCell>) {
        cell.dirty.set(true);
    }

    /// The user pressed "Click to load" on the image `destination` of the active document of
    /// `cell`: the document may load it from now on.
    pub(super) fn allow_remote_image(&self, cell: &Rc<WindowCell>, destination: &str) {
        let doc = cell.view.borrow().active_tab().map(|tab| tab.entry.id());
        if let Some(doc) = doc {
            self.remote_images.allow_url(doc, destination);
            self.remote_images_changed(cell);
        }
    }
}

/// The image at `url` as encoded bytes: from the disk cache, or downloaded and cached. Runs on
/// a worker. `allow_http` is set only after the user asked for an `http` image.
///
/// # Errors
/// The message of the failure: the URL is `http` and not allowed, the transport or the content
/// type failed (`ssw_io::remote_images::ImageError`), the state database or the cache folder
/// cannot be used.
pub fn fetch_remote_image(
    http: &dyn Http,
    database: &Path,
    cache_dir: &Path,
    url: &str,
    allow_http: bool,
) -> Result<Vec<u8>, String> {
    let store = Store::open(database).map_err(|error| error.to_string())?;
    let now = i64::try_from(now_millis() / 1000).unwrap_or(i64::MAX);
    let images = RemoteImages::new(cache_dir.to_path_buf());
    match images.load(&store, http, url, allow_http, now) {
        Ok(ImageLoad::Loaded(image)) => Ok(image.bytes),
        Ok(ImageLoad::Blocked) => Err("plain http images need the user's permission".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_io::net::ScriptedHttp;

    const PNG_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: image/png";

    #[test]
    fn destinations_are_remote_when_they_name_http_or_https() {
        assert!(is_remote("https://example.com/a.png"));
        assert!(is_remote("HTTP://example.com/a.png"));
        assert!(is_https("HtTpS://example.com/a.png"));
        assert!(!is_https("http://example.com/a.png"));
        assert!(!is_remote("images/a.png"));
        assert!(!is_remote("C:\\pics\\a.png"));
        assert!(!is_remote("htt"));
        assert!(!is_remote("é"));
    }

    #[test]
    fn the_policy_remembers_opt_outs_and_allowed_urls_per_document() {
        let policy = RemoteImagePolicy::default();
        let (a, b) = (DocId(1), DocId(2));
        let start = policy.epoch();
        assert!(!policy.is_disabled(a));
        policy.set_disabled(a, true);
        assert!(policy.is_disabled(a) && !policy.is_disabled(b));
        assert_eq!(policy.epoch(), start + 1);
        // Repeating a decision is not a change.
        policy.set_disabled(a, true);
        assert_eq!(policy.epoch(), start + 1);
        policy.set_disabled(a, false);
        assert!(!policy.is_disabled(a));
        assert_eq!(policy.epoch(), start + 2);
        policy.set_disabled(a, false);
        assert_eq!(policy.epoch(), start + 2);

        assert!(!policy.url_allowed(a, "http://example.com/x.png"));
        policy.allow_url(a, "http://example.com/x.png");
        assert!(policy.url_allowed(a, "http://example.com/x.png"));
        assert!(!policy.url_allowed(b, "http://example.com/x.png"));
        assert!(!policy.url_allowed(a, "http://example.com/y.png"));
        assert_eq!(policy.epoch(), start + 3);
        policy.allow_url(a, "http://example.com/x.png");
        assert_eq!(policy.epoch(), start + 3);
    }

    #[test]
    fn an_image_is_downloaded_once_and_then_served_from_the_disk_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (database, cache) = (tmp.path().join("state.db"), tmp.path().join("images"));
        let http = ScriptedHttp::new();
        http.respond("https://example.com/a.png", PNG_HEAD, b"not really a png");
        let first =
            fetch_remote_image(&http, &database, &cache, "https://example.com/a.png", false);
        assert_eq!(first.unwrap(), b"not really a png");
        assert_eq!(http.requests(), ["https://example.com/a.png"]);
        assert_eq!(
            std::fs::read_dir(&cache).unwrap().count(),
            1,
            "cached on disk"
        );
        // The second request is answered by the cache: the script has nothing left to say.
        let second =
            fetch_remote_image(&http, &database, &cache, "https://example.com/a.png", false);
        assert_eq!(second.unwrap(), b"not really a png");
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn plain_http_needs_permission_and_other_content_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (database, cache) = (tmp.path().join("state.db"), tmp.path().join("images"));
        let http = ScriptedHttp::new();
        let blocked =
            fetch_remote_image(&http, &database, &cache, "http://example.com/a.png", false);
        assert!(blocked.unwrap_err().contains("permission"));
        assert!(http.requests().is_empty(), "nothing was requested");

        http.respond("http://example.com/a.png", PNG_HEAD, b"bytes");
        let allowed =
            fetch_remote_image(&http, &database, &cache, "http://example.com/a.png", true);
        assert_eq!(allowed.unwrap(), b"bytes");

        http.respond(
            "https://example.com/page",
            "HTTP/1.1 200 OK\r\nContent-Type: text/html",
            b"<html>",
        );
        let page = fetch_remote_image(&http, &database, &cache, "https://example.com/page", false);
        assert!(page.is_err(), "only image content types are accepted");

        // A state database that cannot be opened (a folder) stops the fetch before any request.
        let before = http.requests().len();
        let unusable = fetch_remote_image(
            &http,
            tmp.path(),
            &cache,
            "https://example.com/b.png",
            false,
        );
        assert!(unusable.is_err());
        assert_eq!(
            http.requests().len(),
            before,
            "no state database, no download"
        );
    }
}
