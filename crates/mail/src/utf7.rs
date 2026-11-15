//! IMAP's modified UTF-7 for folder names (RFC 3501 §5.1.3): `Entwürfe` is `Entw&APw-rfe`.
//! Names are shown decoded and encoded again when the model names a folder.

use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};

fn engine() -> Option<GeneralPurpose> {
    let alphabet = Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,").ok()?;
    let config = GeneralPurposeConfig::new().with_encode_padding(false).with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    Some(GeneralPurpose::new(&alphabet, config))
}

/// A folder name from the server, readable (undecodable runs are kept as they are).
pub fn decode(name: &str) -> String {
    let Some(engine) = engine() else { return name.to_owned() };
    let mut out = String::new();
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at + 1..).unwrap_or_default();
        let Some(end) = after.find('-') else {
            out.push_str(rest.get(at..).unwrap_or_default());
            return out;
        };
        let run = after.get(..end).unwrap_or_default();
        if run.is_empty() {
            out.push('&');
        } else {
            let units: Option<Vec<u16>> =
                engine.decode(run).ok().filter(|b| b.len() % 2 == 0).map(|b| b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect());
            match units.and_then(|u| String::from_utf16(&u).ok()) {
                Some(s) => out.push_str(&s),
                None => {
                    out.push('&');
                    out.push_str(run);
                    out.push('-');
                }
            }
        }
        rest = after.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// A folder name for the server.
pub fn encode(name: &str) -> String {
    let Some(engine) = engine() else { return name.to_owned() };
    let mut out = String::new();
    let mut run: Vec<u16> = Vec::new();
    let flush = |run: &mut Vec<u16>, out: &mut String| {
        if !run.is_empty() {
            let bytes: Vec<u8> = run.iter().flat_map(|u| u.to_be_bytes()).collect();
            out.push('&');
            out.push_str(&engine.encode(bytes));
            out.push('-');
            run.clear();
        }
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut run, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for (plain, wire) in
            [("INBOX", "INBOX"), ("Entwürfe", "Entw&APw-rfe"), ("A&B", "A&-B"), ("日本語", "&ZeVnLIqe-"), ("Gesendete Objekte", "Gesendete Objekte")]
        {
            assert_eq!(encode(plain), wire);
            assert_eq!(decode(wire), plain);
        }
        assert_eq!(decode("bad&!!!-x&"), "bad&!!!-x&");
    }
}
