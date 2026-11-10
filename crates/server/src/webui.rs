//! Is the browser UI we'd serve built for this engine's protocol? `cargo xtask web` stamps
//! `<meta name="byob-protocol" content="N">` into its `index.html`; a UI from an older build
//! would only ever say "protocol mismatch … reload the page", which reloading can't fix.

use std::path::Path;

use byob_protocol::PROTOCOL_VERSION;

/// What the stamp in the UI's `index.html` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCheck {
    /// Built for this protocol (or there's no UI to check).
    Fine,
    /// Built before UIs were stamped: maybe fine, maybe stale.
    Unstamped,
    /// Built for another protocol: it can't talk to this engine.
    Stale { built_for: u32 },
}

impl UiCheck {
    /// Check the UI that will be served: `web_dir`'s, else the one compiled in.
    pub fn of(web_dir: Option<&Path>) -> Self {
        let html = match web_dir {
            Some(dir) => std::fs::read_to_string(dir.join("index.html")).ok(),
            None => embedded_index(),
        };
        html.map_or(Self::Fine, |h| Self::of_html(&h))
    }

    pub fn of_html(html: &str) -> Self {
        match stamped_protocol(html) {
            None => Self::Unstamped,
            Some(v) if v == PROTOCOL_VERSION => Self::Fine,
            Some(built_for) => Self::Stale { built_for },
        }
    }

    /// One line for the terminal, if there's something to say.
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Fine => None,
            Self::Unstamped => Some(
                "the browser UI has no protocol stamp, so it may be out of date; if the page says \"protocol mismatch\", run `cargo xtask web` and rebuild"
                    .into(),
            ),
            Self::Stale { built_for } => Some(format!(
                "the browser UI was built for protocol {built_for}, but this engine speaks {PROTOCOL_VERSION}; it won't be served. Run `cargo xtask web` and rebuild (or point --web-dir at a current build)"
            )),
        }
    }
}

/// The protocol version in `<meta name="byob-protocol" content="N">`, if any.
pub fn stamped_protocol(html: &str) -> Option<u32> {
    let at = html.find("name=\"byob-protocol\"")?;
    let tag_end = html.get(at..)?.find('>')?;
    let tag = html.get(at..at.saturating_add(tag_end))?;
    let content = tag.split("content=\"").nth(1)?;
    content.split('"').next()?.trim().parse().ok()
}

#[cfg(feature = "embedded-web")]
fn embedded_index() -> Option<String> {
    byob_web_assets::get("index.html").filter(|a| !a.gzip).and_then(|a| String::from_utf8(a.bytes.to_vec()).ok())
}

#[cfg(not(feature = "embedded-web"))]
fn embedded_index() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_decides() {
        let page = |v: u32| format!("<head><meta charset=\"utf-8\" />\n<meta name=\"byob-protocol\" content=\"{v}\" /></head>");
        assert_eq!(stamped_protocol(&page(7)), Some(7));
        assert_eq!(UiCheck::of_html(&page(PROTOCOL_VERSION)), UiCheck::Fine);
        assert_eq!(UiCheck::of_html(&page(7)), UiCheck::Stale { built_for: 7 });
        assert!(UiCheck::of_html(&page(7)).warning().is_some_and(|w| w.contains("protocol 7") && w.contains("cargo xtask web")));
        assert_eq!(UiCheck::of_html("<head><title>byob</title></head>"), UiCheck::Unstamped);
        assert_eq!(UiCheck::of_html("<meta name=\"byob-protocol\" content=\"x\">"), UiCheck::Unstamped);
    }

    #[test]
    fn a_web_dir_is_read_and_no_ui_is_fine() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("index.html"), "<meta name=\"byob-protocol\" content=\"3\">").unwrap();
        assert_eq!(UiCheck::of(Some(d.path())), UiCheck::Stale { built_for: 3 });
        assert_eq!(UiCheck::of(Some(&d.path().join("missing"))), UiCheck::Fine);
    }
}
