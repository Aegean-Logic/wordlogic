//! "Chat about this file": the document's conversation in Nextcloud Talk, opened in the browser,
//! as Word opens a file's Teams chat. A native chat panel can come later; the browser already
//! has Talk's chat and calls.
//!
//! The host's cloud ([`crate::cloud::Cloud`]) says whether its server has Talk (`talk`) and
//! answers one operation about the document that's open:
//!
//! - `talk.open` `{}` → `{url}`: opens the document's conversation in the browser; an error saying
//!   to share the document first when Talk has no conversation for it (it isn't shared yet)

use serde_json::{Value as Json, json};

use crate::WordApp;
use crate::cloud::next_request_id;

/// The chat being opened, if it is.
#[derive(Clone, Debug, Default)]
pub struct Talk {
    asked: Option<u64>,
}

impl Talk {
    /// The chat is being opened (the host hasn't answered yet).
    pub fn opening(&self) -> bool {
        self.asked.is_some()
    }
}

/// Whether "Chat about this file" is offered: the document is in Nextcloud and its server has
/// Talk.
pub fn available(app: &WordApp) -> bool {
    app.services.cloud.as_ref().is_some_and(|c| c.has_talk()) && app.cloud_location().is_some()
}

/// Opens the document's conversation in the browser (the host's answer arrives later).
pub fn open(app: &mut WordApp) -> Result<(), String> {
    if !available(app) {
        return Err("Chat is for documents in Nextcloud, on a server with Nextcloud Talk.".into());
    }
    if app.talk.opening() {
        return Ok(());
    }
    let Some(cloud) = &app.services.cloud else { return Err("Chat needs a Nextcloud connection.".into()) };
    let id = next_request_id();
    (cloud.request)(id, "talk.open", json!({}));
    app.talk.asked = Some(id);
    app.status(tl!("Opening the chat in your browser…"));
    Ok(())
}

/// Takes the host's answer. Every frame.
pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    let Some(id) = app.talk.asked else { return };
    let Some(cloud) = &app.services.cloud else {
        app.talk.asked = None;
        return;
    };
    let answer: Option<Result<Json, String>> = {
        let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.iter().position(|(i, _)| *i == id).map(|at| inbox.remove(at).1)
    };
    let Some(answer) = answer else { return };
    app.talk.asked = None;
    match answer {
        Ok(_) => app.status(tl!("The chat opened in your browser.")),
        Err(e) => app.status(e),
    }
    ctx.request_repaint();
}
