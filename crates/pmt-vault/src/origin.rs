//! Exact web origins (`scheme://host[:port]`): a vault item fills only on
//! a page whose origin equals one of its saved origins, so
//! `https://github.com.evil.example` never gets a `https://github.com`
//! login.

/// The serialized origin of `url` (`https://github.com` for
/// `https://GitHub.com:443/login`); `None` for a URL without a tuple
/// origin (`about:blank`, `data:`, `file:`).
#[must_use]
pub fn of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let origin = parsed.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Whether the page at `page_url` may receive an item saved for `saved`.
#[must_use]
pub fn admits(saved: &[String], page_url: &str) -> bool {
    let Some(page) = of(page_url) else {
        return false;
    };
    saved.iter().filter_map(|s| of(s)).any(|s| s == page)
}

#[cfg(test)]
mod tests {
    use super::{admits, of};

    #[test]
    fn origins_match_exactly() {
        let saved = vec!["https://github.com".to_owned()];
        assert!(admits(&saved, "https://github.com/login?x=1"));
        assert!(admits(&saved, "https://GITHUB.com:443/"));
        assert!(!admits(&saved, "https://github.com.evil.example/login"));
        assert!(!admits(&saved, "http://github.com/login"));
        assert!(!admits(&saved, "https://gist.github.com/"));
        assert!(!admits(&saved, "https://github.com:8443/"));
        assert!(!admits(&saved, "about:blank"));
        assert_eq!(
            of("http://127.0.0.1:8080/x").as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }
}
