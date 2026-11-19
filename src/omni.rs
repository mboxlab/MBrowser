//! Address bar: input resolution, suggestions and inline completion.

use std::collections::HashSet;

use serde::Serialize;

use crate::store::{Bookmarks, History, Settings, now};

#[derive(Serialize)]
pub struct Sugg {
    pub kind: &'static str,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
}

#[derive(Serialize)]
pub struct Completion {
    pub text: String,
    pub url: String,
}

pub struct TabInfo<'a> {
    pub id: u32,
    pub title: &'a str,
    pub url: &'a str,
}

/// Turns address bar text into a URL: either the address itself or a search.
pub fn resolve(input: &str, settings: &Settings) -> Option<String> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    Some(as_url(t).unwrap_or_else(|| settings.search_url(t)))
}

/// Allowed for direct navigation (no javascript:, data: etc. from the bar).
pub fn is_navigable(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["http://", "https://", "file://", "about:", "edge://", "view-source:"].iter().any(|p| l.starts_with(p))
}

/// The input as a site address, if it looks like one (no search fallback).
pub fn as_url(t: &str) -> Option<String> {
    if t.contains(char::is_whitespace) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if is_navigable(&lower) {
        return Some(t.to_string());
    }
    if lower.contains("://") {
        return None;
    }
    let host_port = lower.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.rsplit_once(':').map_or(host_port, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) { h } else { host_port }
    });
    if host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(format!("http://{t}"));
    }
    let tld = host.rsplit('.').next().unwrap_or("");
    let looks_like_domain = host.contains('.')
        && !host.starts_with('.')
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_alphabetic())
        && host.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.');
    looks_like_domain.then(|| format!("https://{t}"))
}

/// "https://www.Example.com/x" -> "example.com/x"
fn strip(url: &str) -> &str {
    let u = url.split_once("://").map_or(url, |(_, r)| r);
    u.strip_prefix("www.").unwrap_or(u)
}

pub fn suggest(
    q: &str,
    tabs: &[TabInfo],
    history: &History,
    bookmarks: &Bookmarks,
    settings: &Settings,
) -> (Vec<Sugg>, Option<Completion>) {
    let q = q.trim();
    if q.is_empty() {
        return (Vec::new(), None);
    }
    let ql = q.to_lowercase();
    let tokens: Vec<&str> = ql.split_whitespace().collect();
    let matches = |title: &str, url: &str| {
        let (t, u) = (title.to_lowercase(), url.to_lowercase());
        tokens.iter().all(|tk| t.contains(tk) || u.contains(tk))
    };

    // Score history + bookmarks together.
    let starred: HashSet<&str> = bookmarks.items.iter().map(|b| b.url.as_str()).collect();
    let t_now = now();
    let mut scored: Vec<(f64, &str, &str, bool)> = Vec::new();
    for e in &history.entries {
        if !matches(&e.title, &e.url) {
            continue;
        }
        let stripped = strip(&e.url).to_lowercase();
        let age_days = t_now.saturating_sub(e.last) as f64 / 86_400.0;
        let mut s = (e.visits as f64).ln_1p() * 8.0 + 12.0 / (1.0 + age_days / 7.0);
        if stripped.starts_with(&ql) {
            s += 30.0;
            // Prefer the site root over deep links for inline completion.
            s += 10.0 / (1.0 + stripped.len() as f64 / 20.0);
        }
        let star = starred.contains(e.url.as_str());
        if star {
            s += 15.0;
        }
        scored.push((s, &e.url, &e.title, star));
    }
    for b in &bookmarks.items {
        if matches(&b.title, &b.url) && !scored.iter().any(|x| x.1 == b.url) {
            let bonus = if strip(&b.url).to_lowercase().starts_with(&ql) { 30.0 } else { 0.0 };
            scored.push((25.0 + bonus, &b.url, &b.title, true));
        }
    }
    scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));

    // Inline completion: complete to a host the user visits often.
    let mut completion = None;
    if !q.contains(char::is_whitespace) && !q.contains('/') {
        for (_, url, _, _) in &scored {
            let stripped = strip(url);
            let host = stripped.split(['/', '?', '#']).next().unwrap_or("");
            if host.len() > ql.len() && host.to_lowercase().starts_with(&ql) {
                let scheme_host = url.find(host).map(|i| &url[..i + host.len()]).unwrap_or(url);
                completion = Some(Completion { text: host.to_lowercase(), url: format!("{scheme_host}/") });
                break;
            }
        }
    }

    let mut out: Vec<Sugg> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(c) = &completion {
        seen.insert(c.url.clone());
        out.push(Sugg { kind: "url", title: c.text.clone(), url: c.url.clone(), id: None });
    }
    match as_url(q) {
        Some(url) if completion.is_none() => {
            seen.insert(url.clone());
            out.push(Sugg { kind: "url", title: q.to_string(), url, id: None });
        }
        _ => out.push(Sugg { kind: "search", title: q.to_string(), url: settings.search_url(q), id: None }),
    }
    for t in tabs.iter().filter(|t| matches(t.title, t.url)).take(3) {
        seen.insert(t.url.to_string());
        out.push(Sugg { kind: "tab", title: t.title.to_string(), url: t.url.to_string(), id: Some(t.id) });
    }
    for (_, url, title, star) in scored {
        if out.len() >= 9 {
            break;
        }
        if seen.insert(url.to_string()) {
            let kind = if star { "bookmark" } else { "history" };
            out.push(Sugg { kind, title: title.to_string(), url: url.to_string(), id: None });
        }
    }
    (out, completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_input() {
        let s = Settings::default();
        assert_eq!(resolve("example.com", &s).unwrap(), "https://example.com");
        assert_eq!(resolve("localhost:3000/x", &s).unwrap(), "http://localhost:3000/x");
        assert_eq!(resolve("192.168.1.1", &s).unwrap(), "http://192.168.1.1");
        assert_eq!(resolve("https://a.b/c d", &s).unwrap().starts_with("https://www.google.com"), true);
        assert!(resolve("rust lang", &s).unwrap().contains("q=rust+lang"));
        assert!(resolve("file.txt", &s).unwrap().starts_with("https://file.txt"));
        assert!(resolve("привет", &s).unwrap().starts_with("https://www.google.com/search?q=%D0"));
        assert!(resolve("javascript:alert(1)", &s).unwrap().starts_with("https://www.google.com"));
        assert_eq!(resolve("яндекс.рф", &s).unwrap(), "https://яндекс.рф");
    }
}
