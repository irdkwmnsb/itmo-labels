//! Percent-encoding for the one thing the web client puts in a URL: a printer name.
//! The client encodes, the server decodes.

/// Everything but unreserved characters is escaped, so any name survives a query string.
#[cfg(any(test, target_arch = "wasm32"))]
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `None` for broken escapes or bytes that are not UTF-8.
#[cfg(not(target_arch = "wasm32"))]
pub fn decode(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let [b, tail @ ..] = rest {
        match b {
            b'%' => {
                let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                rest = &tail[2..];
            }
            b'+' => {
                bytes.push(b' ');
                rest = tail;
            }
            _ => {
                bytes.push(*b);
                rest = tail;
            }
        }
    }
    String::from_utf8(bytes).ok()
}

/// The decoded value of `key` in a `a=1&b=2` query string.
#[cfg(not(target_arch = "wasm32"))]
pub fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key { decode(v) } else { None }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_any_name() {
        for name in ["usb 353d:1154 Printer", "usblp /dev/usb/lp0", "Принтер №1 & co=2%"] {
            let encoded = encode(name);
            assert!(encoded.bytes().all(|b| b.is_ascii_graphic() && b != b'&' && b != b'='), "{encoded}");
            assert_eq!(decode(&encoded).as_deref(), Some(name));
        }
    }

    #[test]
    fn broken_escapes_are_rejected() {
        assert_eq!(decode("%zz"), None);
        assert_eq!(decode("ab%4"), None);
        assert_eq!(decode("%FF"), None, "not UTF-8");
        assert_eq!(decode("a+b").as_deref(), Some("a b"));
    }

    #[test]
    fn finds_params() {
        let q = format!("x=1&printer={}&flag", encode("a&b"));
        assert_eq!(param(&q, "printer").as_deref(), Some("a&b"));
        assert_eq!(param(&q, "flag").as_deref(), Some(""));
        assert_eq!(param(&q, "missing"), None);
    }
}
