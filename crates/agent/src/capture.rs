//! The `ui_screenshot` tool: the model sees the byob window showing its chat, to check what
//! it changed with `ui_look` (a backdrop, colors, the arrangement) or what the user means.
//!
//! The engine asks one window showing the chat (cap [`CAP_CAPTURE`]; the one whose look
//! changed last) for a screenshot ([`ServerMsg::Capture`]); it answers with a PNG or JPEG
//! ([`ClientMsg::Captured`](byob_protocol::ClientMsg::Captured)), which is checked and
//! scaled again here, since it came from a client.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use byob_protocol::ServerMsg;
use byob_protocol::look::{CAP_CAPTURE, CAPTURE_SIDE};
use byob_protocol::media::{self, MAX_IMAGE_BYTES};
use byob_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::{ClientId, Engine, Input};

/// The tool's name.
pub const TOOL: &str = "ui_screenshot";

/// How long the tool waits for the screenshot.
const TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(400) } else { Duration::from_secs(10) };

/// A screenshot as a `data:` URL, with its size; or why there's none.
pub(crate) type Answer = Result<(String, (u32, u32)), String>;

/// A screenshot asked of a window.
pub(crate) struct Waiting {
    client: ClientId,
    reply: Sender<Answer>,
}

const NO_WINDOW: &str =
    "no byob window that can take screenshots is showing this chat right now (none open, or one too old to take them), so there's nothing to look at";

impl Engine {
    /// `Input::Capture`: ask the window showing `chat` whose look changed last.
    pub(crate) fn tool_capture(&mut self, chat: &str, reply: Sender<Answer>) {
        let window = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached && c.chat.as_deref() == Some(chat) && c.caps.iter().any(|c| c == CAP_CAPTURE))
            .max_by_key(|(_, c)| c.look.as_ref().map_or(0, |(seq, _)| *seq))
            .map(|(id, _)| *id);
        let Some(client) = window else {
            let _ = reply.send(Err(NO_WINDOW.into()));
            return;
        };
        self.look_seq += 1;
        let request = self.look_seq;
        while self.capture_waits.len() > 8 {
            self.capture_waits.pop_first();
        }
        self.capture_waits.insert(request, Waiting { client, reply });
        self.send_to(client, ServerMsg::Capture { request });
    }

    /// `ClientMsg::Captured`: check the image and hand it to the tool that asked.
    pub(crate) fn captured(&mut self, client: ClientId, request: u64, image: Option<String>, error: Option<String>) {
        if self.capture_waits.get(&request).is_none_or(|w| w.client != client) {
            return;
        }
        let Some(w) = self.capture_waits.remove(&request) else { return };
        let answer = match (image, error) {
            (Some(b64), _) => checked(&b64),
            (None, Some(e)) => Err(format!("the window couldn't take a screenshot: {}", e.chars().take(300).collect::<String>())),
            (None, None) => Err("the window sent no screenshot".into()),
        };
        let _ = w.reply.send(answer);
    }
}

/// The client's image, decoded, scaled to [`CAPTURE_SIDE`] and re-encoded (refusing what
/// isn't a PNG or JPEG, or is too big), as a `data:` URL.
fn checked(b64: &str) -> Answer {
    if b64.len() > MAX_IMAGE_BYTES.saturating_mul(4) / 3 + 4 {
        return Err("the window's screenshot was too big".into());
    }
    let bytes = media::decode(b64).map_err(|e| format!("the window's screenshot didn't decode: {e}"))?;
    let (mime, small) = byob_tools::images::fit(bytes, CAPTURE_SIDE, MAX_IMAGE_BYTES).map_err(|e| format!("the window's screenshot isn't usable: {e}"))?;
    let size = byob_tools::images::dimensions(&small).ok_or("the window's screenshot isn't usable")?;
    Ok((format!("data:{mime};base64,{}", media::encode(&small)), size))
}

fn request(inbox: &Sender<Input>, chat: &str, cancel: &AtomicBool) -> Answer {
    let (reply, answer) = channel();
    inbox.send(Input::Capture { chat: chat.to_owned(), reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < TIMEOUT => {}
            Err(_) => return Err("the window didn't send a screenshot in 10 seconds (it may be a browser tab in the background, which can't draw)".into()),
        }
    }
}

/// The model's tool: a screenshot of the window showing this chat.
pub(crate) struct UiScreenshot {
    pub inbox: Sender<Input>,
}

impl Tool for UiScreenshot {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "See the byob window showing this chat, as a screenshot: to check a look you changed with ui_look (does the backdrop show where you meant, can the text be read over it, does the arrangement fit) or to see what the user is talking about. Shows the whole window: the chat, the side panels and any open settings or artifacts.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn title(&self, _args: &Value) -> String {
        "Took a screenshot of the window".into()
    }

    fn pending_title(&self, _args: &Value) -> String {
        "Taking a screenshot of the window".into()
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, _args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let Some(chat) = ctx.chat() else { return ToolOutput::err(NO_WINDOW) };
        match request(&self.inbox, chat, ctx.cancel_flag()) {
            Ok((url, (w, h))) => {
                ToolOutput { images: vec![url], ..ToolOutput::ok(format!("A screenshot of the byob window showing this chat ({w}×{h} px).")) }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clients_screenshot_is_checked() {
        assert!(checked("bm90IGFuIGltYWdl").is_err(), "not an image");
        assert!(checked("!!!").is_err(), "not base64");
        let mut png = Vec::new();
        image::RgbImage::from_pixel(3000, 1000, image::Rgb([9, 9, 9])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let (url, size) = checked(&media::encode(&png)).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "re-encoded when scaled");
        assert_eq!(size, (CAPTURE_SIDE, 523), "scaled down here too");
    }
}
