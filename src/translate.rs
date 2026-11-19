//! Page translation backend: Google Translate's public keyless web endpoint
//! (the one its own widgets use). One POST translates a batch of HTML
//! fragments; inline tags (our `<a i=N>` markers) survive translation.
//! Called from background threads.

use serde_json::Value;

use crate::net;

/// Translates `texts` (HTML fragments) into `to` ("ru", "en", …), source auto-detected.
pub fn translate(texts: &[String], to: &str) -> Result<Vec<String>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("https://translate.googleapis.com/translate_a/t?client=gtx&sl=auto&tl={}&format=html", net::form_encode(to));
    let body: Vec<String> = texts.iter().map(|t| format!("q={}", net::form_encode(t))).collect();
    let r = net::post(
        &url,
        &[("Content-Type", "application/x-www-form-urlencoded;charset=UTF-8")],
        body.join("&").as_bytes(),
    )?;
    if !r.ok() {
        return Err(format!("HTTP {}", r.status));
    }
    let v: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
    parse(&v, texts.len()).ok_or_else(|| "unexpected answer".into())
}

/// The answer's shape depends on the batch: `[["текст","en"], …]` for several
/// texts with auto-detection, `["текст","en"]` or `"текст"` for one.
fn parse(v: &Value, n: usize) -> Option<Vec<String>> {
    let first = |x: &Value| -> Option<String> {
        match x {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => a.first()?.as_str().map(String::from),
            _ => None,
        }
    };
    match v {
        Value::String(s) if n == 1 => Some(vec![s.clone()]),
        Value::Array(a) if n == 1 && a.first().is_some_and(Value::is_string) => Some(vec![a[0].as_str()?.to_string()]),
        Value::Array(a) if a.len() == n => a.iter().map(first).collect(),
        _ => None,
    }
}

/// A language code we are willing to put into a URL.
pub fn valid_lang(s: &str) -> bool {
    (2..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn parses_all_shapes() {
        let p = super::parse;
        assert_eq!(p(&json!([["Привет", "en"], ["Мир", "en"]]), 2), Some(vec!["Привет".into(), "Мир".into()]));
        assert_eq!(p(&json!(["Привет", "en"]), 1), Some(vec!["Привет".into()]));
        assert_eq!(p(&json!("Привет"), 1), Some(vec!["Привет".into()]));
        assert_eq!(p(&json!(["А", "Б"]), 2), Some(vec!["А".into(), "Б".into()]));
        assert_eq!(p(&json!([["А", "en"]]), 2), None);
        assert!(super::valid_lang("ru") && super::valid_lang("zh-CN") && !super::valid_lang("r&u"));
    }

    /// Network: `cargo test translate -- --ignored`.
    #[test]
    #[ignore]
    fn translates_a_batch() {
        let out = super::translate(&["Hello world".into(), "<a i=0>Sign</a><a i=1> in</a>".into()], "ru").unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].to_lowercase().contains("мир"), "{out:?}");
        assert!(out[1].contains("<a i=0>"), "{out:?}");
    }
}
