//! Minimal blocking HTTP(S) client on top of WinHTTP: the system TLS stack,
//! proxy settings and gzip, no extra crates. Call it from background threads only.

use std::sync::OnceLock;

use windows::{
    Win32::Networking::WinHttp::*,
    core::{HSTRING, PCWSTR},
};

/// Browser-like identity: some services answer bots differently.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// WinHTTP handles are thread-safe; one session serves every thread.
struct Session(*mut core::ffi::c_void);
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

fn session() -> Option<*mut core::ffi::c_void> {
    static S: OnceLock<Session> = OnceLock::new();
    let s = S.get_or_init(|| unsafe {
        let h = WinHttpOpen(
            &HSTRING::from(USER_AGENT),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if !h.is_null() {
            let _ = WinHttpSetTimeouts(h, 10_000, 10_000, 20_000, 30_000);
            // gzip + deflate, decoded transparently.
            let _ = WinHttpSetOption(Some(h), WINHTTP_OPTION_DECOMPRESSION, Some(&3u32.to_le_bytes()));
            let _ = WinHttpSetOption(Some(h), WINHTTP_OPTION_ENABLE_HTTP_PROTOCOL, Some(&WINHTTP_PROTOCOL_FLAG_HTTP2.to_le_bytes()));
        }
        Session(h)
    });
    (!s.0.is_null()).then_some(s.0)
}

struct Handle(*mut core::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// (https, host, port, path+query)
fn split(url: &str) -> Option<(bool, &str, u16, &str)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        (false, url.strip_prefix("http://")?)
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (auth, path) = rest.split_at(end);
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || p.parse::<u16>().is_ok() => (h, p.parse().ok()?),
        _ => (auth, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some((secure, host, port, if path.is_empty() { "/" } else { path }))
}

pub fn get(url: &str) -> Result<Response, String> {
    request("GET", url, &[], None)
}

pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    request("POST", url, headers, Some(body))
}

pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Response, String> {
    let (secure, host, port, path) = split(url).ok_or("bad url")?;
    let sess = session().ok_or("WinHttpOpen failed")?;
    let err = |what: &str| format!("{what}: {}", windows::core::Error::from_thread());
    unsafe {
        let conn = Handle(WinHttpConnect(sess, &HSTRING::from(host), port, 0));
        if conn.0.is_null() {
            return Err(err("connect"));
        }
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            &HSTRING::from(method),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags | WINHTTP_FLAG_REFRESH,
        ));
        if req.0.is_null() {
            return Err(err("open request"));
        }
        let mut head = String::new();
        for (k, v) in headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        let head: Vec<u16> = head.encode_utf16().collect();
        let body = body.unwrap_or(&[]);
        WinHttpSendRequest(
            req.0,
            (!head.is_empty()).then_some(&head[..]),
            (!body.is_empty()).then_some(body.as_ptr().cast()),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .map_err(|e| format!("send: {e}"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;

        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("status: {e}"))?;

        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut n = 0u32;
            WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > 64 << 20 {
                return Err("response too large".into());
            }
        }
        Ok(Response { status, body: out })
    }
}

/// `application/x-www-form-urlencoded` value.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn splits_urls() {
        assert_eq!(super::split("https://a.b/c?d"), Some((true, "a.b", 443, "/c?d")));
        assert_eq!(super::split("http://a.b:8080"), Some((false, "a.b", 8080, "/")));
        assert_eq!(super::split("https://a.b?x=1"), Some((true, "a.b", 443, "?x=1")));
        assert_eq!(super::split("ftp://a.b/"), None);
    }

    /// Network: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn fetches_https() {
        let r = super::get("https://ublockorigin.github.io/uAssetsCDN/filters/quick-fixes.min.txt").unwrap();
        assert!(r.ok(), "status {}", r.status);
        assert!(r.text().contains("! Title"));
    }
}
