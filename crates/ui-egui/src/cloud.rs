//! The host's cloud (Nextcloud), as OneDrive is Word's: the File view's Nextcloud places
//! ([`crate::cloud_files`]) and the Share dialog talk to it through [`Cloud`]. The host does the
//! talking (the desktop app over HTTP, the browser page through the host bridge); the UI only
//! sends requests and reads the answers.
//!
//! These are GridLogic's definitions (`gridcraft/crates/ui-egui/src/share.rs`): the Share dialog
//! ported from there ([`crate::share`]) and the File view share this one cloud.

use std::sync::{Arc, Mutex};

use serde_json::Value as Json;

/// Answers from the cloud: (request id, result).
pub type CloudInbox = Arc<Mutex<Vec<(u64, Result<Json, String>)>>>;

/// The host's cloud. `request(id, op, params)` starts an operation and returns at once; its
/// answer arrives in `inbox` under the same id (the host asks for a repaint). Operations about
/// "the document" are about the one open in the window:
///
/// - `share.info` `{}` → `{name, signedIn, inCloud, canShare, account?, server?, link?}`; `link`
///   when the document was opened from a shared link (its address), by someone who can't share it
/// - `share.search` `{query}` → `[{label, detail?, type, shareWith}]`, `type` one of `user`,
///   `group`, `email`, `remote`
/// - `share.list` `{}` → `[{id, type, shareWith, label, edit, url?, password, expiration?}]`,
///   `type` also `link`
/// - `share.add` `{type, shareWith, edit, note}` → a share
/// - `share.update` `{id, edit?, password?, expiration?}` → the share
/// - `share.remove` `{id}` → `{}`
/// - `share.link` `{edit, password?, expiration?}` → the link share (with `url`), made or updated
/// - `share.upload` `{}` → info: puts a document that isn't in the cloud yet there (desktop)
/// - `account.signIn` `{server}` → info: signs in to the cloud at that address (desktop)
///
/// With `files` (the desktop), the File view's operations too ([`crate::cloud_files`]):
///
/// - `account.info` `{}` → `{signedIn, account?, server?}`; `account.signOut` `{}` → the same
/// - `files.list` `{path}` → `{path, items: [{name, path, folder, size}]}`
/// - `files.open` `{path, openPaths}` → `{localPath, alreadyOpen?}`: a copy here, tied to the
///   cloud file (open it like any file; saving it saves to the cloud)
/// - `files.saveAs` `{folder, name, replace}` → `{localPath, name}` (save the document there; it
///   goes to the cloud), or `{exists: true, name}`
/// - `files.resolve` `{localPath, choice}` → what to do after a sync conflict
///   ([`crate::cloud_files::Conflict`]): `choice` `keepBoth` → `{localPath, saveAs: true, name}`,
///   `mine` → `{name}`, `theirs` → `{localPath, reload: true}`
///
/// With `talk`, chat about the document ([`crate::talk`]): `talk.open` `{}` → `{url}`.
pub struct Cloud {
    pub request: Box<dyn Fn(u64, &str, Json)>,
    pub inbox: CloudInbox,
    /// The File view can open and save cloud files (account, Open, Save As).
    pub files: bool,
    /// The cloud file a local file is the copy of (`folder/name.docx`), if it is one: such
    /// documents AutoSave by default.
    pub location: Option<Box<dyn Fn(&str) -> Option<String>>>,
    /// Whether the cloud's server has chat (Nextcloud Talk), for "Chat about this file"; asked
    /// every frame, so it answers from what the host already knows.
    pub talk: Option<Box<dyn Fn() -> bool>>,
}

impl Cloud {
    /// A cloud for sharing only (the browser's: the page owns the files).
    pub fn sharing(request: Box<dyn Fn(u64, &str, Json)>, inbox: CloudInbox) -> Cloud {
        Cloud { request, inbox, files: false, location: None, talk: None }
    }

    /// The cloud file `local` is the copy of, if any.
    pub fn location_of(&self, local: &str) -> Option<String> {
        self.location.as_ref().and_then(|f| f(local))
    }

    /// Whether the cloud's server has chat (Nextcloud Talk).
    pub fn has_talk(&self) -> bool {
        self.talk.as_ref().is_some_and(|f| f())
    }
}

/// A request id no other cloud request in this app uses (answers share one inbox).
pub fn next_request_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
