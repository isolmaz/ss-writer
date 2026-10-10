//! Heading anchors, shared by the ODT reader (links to headings) and writer (bookmarks).

use std::collections::HashMap;

fn is_combining(c: char) -> bool {
    matches!(u32::from(c), 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// GitHub-style anchor of a heading: lower case; letters, digits, `-`, `_` and the combining
/// marks attached to them are kept, each space becomes `-`, everything else is dropped.
pub(super) fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut attached = false;
    for c in title.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            slug.push(c);
            attached = true;
        } else if c == ' ' {
            slug.push('-');
            attached = false;
        } else if attached && is_combining(c) {
            slug.push(c);
        } else {
            attached = false;
        }
    }
    slug.to_lowercase()
}

/// A slug made unique the way GitHub does: a repeated slug gets `-1`, `-2`, …
pub(super) fn unique_slug(title: &str, used: &mut HashMap<String, usize>) -> String {
    let base = slugify(title);
    let mut slug = base.clone();
    while used.contains_key(&slug) {
        let count = used.entry(base.clone()).or_insert(0);
        *count += 1;
        slug = format!("{base}-{count}");
    }
    used.insert(slug.clone(), 0);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_github() {
        let cases = [
            ("Hello, World!", "hello-world"),
            ("A  b", "a--b"),
            ("snake_case-and 42", "snake_case-and-42"),
            ("Ünïcödé Çafé", "ünïcödé-çafé"),
            ("Cafe\u{301}", "cafe\u{301}"),
            ("!\u{301}a", "a"),
            ("😀", ""),
            ("", ""),
        ];
        for (title, slug) in cases {
            assert_eq!(slugify(title), slug, "{title:?}");
        }
    }

    #[test]
    fn repeated_slugs_get_numeric_suffixes() {
        let mut used = HashMap::new();
        let slugs: Vec<String> = ["A", "A", "a", "A 1", "A-1", "A"]
            .iter()
            .map(|t| unique_slug(t, &mut used))
            .collect();
        assert_eq!(slugs, ["a", "a-1", "a-2", "a-1-1", "a-1-2", "a-3"]);
        let empty: Vec<String> = ["😀", "😀"]
            .iter()
            .map(|t| unique_slug(t, &mut HashMap::new()))
            .collect();
        assert_eq!(empty, ["", ""], "separate maps do not collide");
        let mut shared = HashMap::new();
        assert_eq!(unique_slug("😀", &mut shared), "");
        assert_eq!(unique_slug("😀", &mut shared), "-1");
    }
}
