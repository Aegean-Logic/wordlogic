//! Nextcloud in the File view, as OneDrive is in Word's: the Account page signs in and out, Open
//! and Save As browse Nextcloud's folders, and Recent reopens a document from Nextcloud. The
//! host's cloud ([`crate::cloud::Cloud`] with `files`, the desktop) does the talking; this keeps
//! what the pages show and turns the answers into actions. A document opened from (or saved to)
//! Nextcloud is a local copy tied to the Nextcloud file, so opening and saving it are the
//! ordinary `file.open` and `file.save`.

use std::collections::HashMap;

use egui::{Align2, Rect, RichText, Sense, Ui, pos2, vec2};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::cloud::next_request_id;
use crate::theme::{Tokens, medium, regular, semibold};
use crate::{WordApp, icons};

/// The signed-in account, as the cloud describes it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub signed_in: bool,
    /// `ana on cloud.example.com`.
    pub account: Option<String>,
    pub server: Option<String>,
}

/// A file or folder in a listing.
#[derive(Clone, Debug, Deserialize)]
pub struct Item {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub folder: bool,
}

/// A document whose Nextcloud file someone else changed while it was being worked on here; the
/// user chooses what to keep. Pushed by the host.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// The local copy (the document's path).
    pub local: String,
    /// The file's name in Nextcloud.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Listing {
    path: String,
    #[serde(default)]
    items: Vec<Item>,
}

/// What a request in flight is for.
#[derive(Clone, Debug)]
enum Asked {
    Account,
    SignIn,
    SignOut,
    List(String),
    Open(String),
    SaveAs,
    Resolve(String),
}

/// The File view's Nextcloud state (on [`WordApp`], so answers that arrive after the view was
/// closed still open or save the document).
#[derive(Default)]
pub struct CloudFiles {
    /// `None` until the cloud said.
    pub account: Option<Account>,
    /// The server address typed to sign in.
    pub server: String,
    /// The folder shown, relative to the user's files.
    pub folder: String,
    /// Its contents, once listed.
    pub items: Option<Vec<Item>>,
    /// Save As: the name typed, and the document ([`wordcraft_engine::Session::document_id`]) it
    /// was first filled in for.
    pub name: String,
    named_for: Option<u64>,
    /// Save As: a file of this name is there already; waiting for Replace.
    pub replace: Option<String>,
    pub status: Option<(String, bool)>,
    /// Documents waiting for the user to choose between their changes and Nextcloud's.
    pub conflicts: Vec<Conflict>,
    /// A problem to tell the user about wherever they are (title, text): pushed by the host, or a
    /// request asked from the File view that failed after it was closed.
    pub notice: Option<(String, String)>,
    asked: HashMap<u64, Asked>,
}

/// Whether the File view shows Nextcloud (the host's cloud can open and save files).
pub fn available(app: &WordApp) -> bool {
    app.services.cloud.as_ref().is_some_and(|c| c.files)
}

fn ask(app: &WordApp, cf: &mut CloudFiles, op: &str, params: Json, what: Asked) {
    let Some(cloud) = &app.services.cloud else { return };
    let id = next_request_id();
    cf.asked.insert(id, what);
    (cloud.request)(id, op, params);
}

impl CloudFiles {
    fn busy(&self) -> bool {
        self.asked.values().any(|a| !matches!(a, Asked::List(_)))
    }
    fn signing_in(&self) -> bool {
        self.asked.values().any(|a| matches!(a, Asked::SignIn))
    }
    fn signed_in(&self) -> bool {
        self.account.as_ref().is_some_and(|a| a.signed_in)
    }
}

/// Every frame: the cloud's answers to the File view's requests.
pub fn poll(app: &mut WordApp) {
    if app.cloud_files.asked.is_empty() {
        return;
    }
    let Some(cloud) = &app.services.cloud else { return };
    let answers: Vec<(u64, Result<Json, String>)> = {
        let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<_>, Vec<_>) = inbox.drain(..).partition(|(id, _)| app.cloud_files.asked.contains_key(id));
        *inbox = rest;
        mine
    };
    for (id, answer) in answers {
        let Some(what) = app.cloud_files.asked.remove(&id) else { continue };
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                // The File view shows it; a document asked for after the view closed says so.
                if !app.ui.backstage && matches!(what, Asked::Open(_) | Asked::SaveAs | Asked::Resolve(_)) {
                    app.cloud_files.notice = Some((tl!("Nextcloud").to_string(), e.clone()));
                }
                app.cloud_files.status = Some((e, true));
                continue;
            }
        };
        match what {
            Asked::Account | Asked::SignIn | Asked::SignOut => {
                let account: Account = serde_json::from_value(answer).unwrap_or_default();
                let cf = &mut app.cloud_files;
                if cf.server.is_empty() {
                    cf.server = account.server.clone().unwrap_or_default();
                }
                if !account.signed_in {
                    cf.items = None;
                    cf.folder.clear();
                }
                cf.status = match what {
                    Asked::SignIn => Some((tl!("Signed in.").to_string(), false)),
                    Asked::SignOut => Some((tl!("Signed out.").to_string(), false)),
                    _ => None,
                };
                cf.account = Some(account);
            }
            Asked::List(path) => {
                if let Ok(listing) = serde_json::from_value::<Listing>(answer)
                    && listing.path.trim_matches('/') == path.trim_matches('/')
                {
                    app.cloud_files.folder = path;
                    app.cloud_files.items = Some(listing.items);
                }
            }
            Asked::Open(name) => {
                let Some(local) = answer.get("localPath").and_then(Json::as_str).map(str::to_string) else { continue };
                if answer.get("alreadyOpen").and_then(Json::as_bool) == Some(true) {
                    app.cloud_files.status = None;
                    leave_backstage(app);
                    continue;
                }
                app.cloud_files.status = Some((crate::i18n::fmt(tl!("Opened {name} from Nextcloud."), &[("name", &name)]), false));
                open_local(app, &local);
            }
            Asked::Resolve(name) => resolved(app, &name, &answer),
            Asked::SaveAs => {
                let name = answer.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
                if answer.get("exists").and_then(Json::as_bool) == Some(true) {
                    app.cloud_files.replace = Some(name);
                    continue;
                }
                let Some(local) = answer.get("localPath").and_then(Json::as_str).map(str::to_string) else { continue };
                // Saved here, then sent to Nextcloud by the host.
                match app.run("file.save", json!({"path": local})) {
                    Ok(_) => {
                        app.cloud_files.status = Some((crate::i18n::fmt(tl!("Saving {name} to Nextcloud…"), &[("name", &name)]), false));
                        leave_backstage(app);
                    }
                    Err(e) => app.cloud_files.notice = Some((tl!("Nextcloud").to_string(), e)),
                }
            }
        }
    }
}

fn leave_backstage(app: &mut WordApp) {
    app.ui.backstage = false;
    app.canvas.want_focus = true;
}

/// Opens a local copy of a Nextcloud file as File › Open opens a file (asking about unsaved
/// changes first), and shows it.
fn open_local(app: &mut WordApp, local: &str) {
    match app.run("file.open", json!({"path": local})) {
        // Waiting for Save / Don't Save / Cancel: the File view stays until it's answered.
        Ok(r) if r.get("pending").is_some() => {}
        Ok(_) => leave_backstage(app),
        Err(e) => app.cloud_files.notice = Some((tl!("Nextcloud").to_string(), e)),
    }
}

/// Acts on the host's answer to a conflict choice.
fn resolved(app: &mut WordApp, name: &str, answer: &Json) {
    let local = answer.get("localPath").and_then(Json::as_str).map(str::to_string);
    if answer.get("reload").and_then(Json::as_bool) == Some(true)
        && let Some(local) = local
    {
        // Nextcloud's version replaced the copy here: open it in place of the document (the
        // changes made here are what the user chose to drop, so nothing asks about them).
        match app.execute("file.open", json!({"path": local})) {
            Ok(_) => app.status(crate::i18n::fmt(tl!("{name}: Nextcloud's version is open."), &[("name", name)])),
            Err(e) => app.cloud_files.notice = Some((tl!("Nextcloud").to_string(), e)),
        }
    } else if answer.get("saveAs").and_then(Json::as_bool) == Some(true)
        && let Some(local) = local
    {
        // Their version stays; this one goes next to it as a copy, which the document becomes.
        let copy = answer.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
        match app.execute("file.save", json!({"path": local})) {
            Ok(_) => {
                app.status(crate::i18n::fmt(tl!("Your changes are in {copy}; {name} keeps Nextcloud's version."), &[("copy", &copy), ("name", name)]))
            }
            Err(e) => app.cloud_files.notice = Some((tl!("Nextcloud").to_string(), e)),
        }
    } else {
        app.status(crate::i18n::fmt(tl!("Your version of {name} is in Nextcloud."), &[("name", name)]));
    }
}

/// Over everything, wherever the user is: the prompt for the first conflict waiting (keep both,
/// keep mine, or take Nextcloud's), and any problem to tell.
pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    show_conflict(app, ctx);
    let Some((title, text)) = app.cloud_files.notice.clone() else { return };
    let mut close = false;
    egui::Modal::new(egui::Id::new("nextcloud_notice")).show(ctx, |ui| {
        let t = Tokens::get(ui.ctx());
        ui.set_width(420.0);
        ui.label(RichText::new(&title).font(semibold(15.0)).color(t.text));
        ui.add_space(6.0);
        ui.label(&text);
        ui.add_space(12.0);
        close = ui.button(tl!("OK")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape));
    });
    if close {
        app.cloud_files.notice = None;
    }
}

fn show_conflict(app: &mut WordApp, ctx: &egui::Context) {
    let Some(c) = app.cloud_files.conflicts.first().cloned() else { return };
    let busy = app.cloud_files.asked.values().any(|a| matches!(a, Asked::Resolve(_)));
    let mut choice = None;
    egui::Modal::new(egui::Id::new("nextcloud_conflict")).show(ctx, |ui| {
        let t = Tokens::get(ui.ctx());
        ui.set_width(480.0);
        ui.label(RichText::new(crate::i18n::fmt(tl!("{name} changed in Nextcloud"), &[("name", &c.name)])).font(semibold(15.0)).color(t.text));
        ui.add_space(6.0);
        ui.label(tl!(
            "Someone else saved this document in Nextcloud while you were working on it here. Your changes are safe on this computer. What should happen to them?"
        ));
        ui.add_space(12.0);
        let options = [
            ("keepBoth", tl!("Keep both"), tl!("Your changes go into a copy next to it; theirs stay in the original.")),
            ("mine", tl!("Use mine"), tl!("Your version replaces theirs (Nextcloud keeps theirs as an older version).")),
            ("theirs", tl!("Use Nextcloud's"), tl!("Your changes are dropped and their version opens here.")),
        ];
        for (id, label, detail) in options {
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(label).min_size(vec2(130.0, 0.0))).clicked() {
                    choice = Some(id);
                }
                ui.label(RichText::new(detail).color(t.text_dim));
            });
            ui.add_space(4.0);
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tl!("Working…"));
            });
        }
    });
    if let Some(choice) = choice {
        let mut cf = std::mem::take(&mut app.cloud_files);
        cf.conflicts.retain(|x| x != &c);
        ask(app, &mut cf, "files.resolve", json!({"localPath": c.local, "choice": choice}), Asked::Resolve(c.name.clone()));
        app.cloud_files = cf;
    }
}

/// Asks the cloud about the account the first time the File view needs it.
fn ensure_account(app: &WordApp, cf: &mut CloudFiles) {
    if cf.account.is_none() && !cf.asked.values().any(|a| matches!(a, Asked::Account)) {
        ask(app, cf, "account.info", json!({}), Asked::Account);
    }
}

/// Lists `cf.folder` once signed in, if it isn't listed or asked for yet.
fn ensure_listing(app: &WordApp, cf: &mut CloudFiles) {
    if cf.signed_in() && cf.items.is_none() && !cf.asked.values().any(|a| matches!(a, Asked::List(_))) {
        let folder = cf.folder.clone();
        ask(app, cf, "files.list", json!({"path": folder}), Asked::List(folder));
    }
}

fn go_to(app: &WordApp, cf: &mut CloudFiles, folder: &str) {
    cf.items = None;
    cf.folder = folder.trim_matches('/').to_string();
    let folder = cf.folder.clone();
    ask(app, cf, "files.list", json!({"path": folder}), Asked::List(folder));
}

fn status(ui: &mut Ui, cf: &CloudFiles) {
    if let Some((text, problem)) = &cf.status {
        let t = Tokens::get(ui.ctx());
        ui.add_space(6.0);
        ui.label(RichText::new(text).color(if *problem { t.red } else { t.text_dim }));
    }
}

/// A section title on a File view page.
fn section(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(semibold(16.0)));
    ui.add_space(6.0);
}

/// File › Account: sign in to Nextcloud, or see who's signed in and sign out.
pub fn account_page(app: &mut WordApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    if !available(app) {
        ui.label(RichText::new(tl!("Nextcloud accounts are managed by the page WordLogic runs in.")).color(t.text_dim));
        return;
    }
    let mut cf = std::mem::take(&mut app.cloud_files);
    ensure_account(app, &mut cf);
    section(ui, tl!("Nextcloud"));
    match cf.account.clone() {
        None => {
            ui.spinner();
        }
        Some(a) if a.signed_in => {
            ui.label(crate::i18n::fmt(tl!("Signed in as {account}."), &[("account", &a.account.unwrap_or_default())]));
            ui.label(
                RichText::new(tl!("Open and Save As show your Nextcloud files; documents there save to Nextcloud as you work.")).color(t.text_dim),
            );
            ui.add_space(10.0);
            if ui.add_enabled(!cf.busy(), egui::Button::new(tl!("Sign out"))).clicked() {
                ask(app, &mut cf, "account.signOut", json!({}), Asked::SignOut);
            }
        }
        Some(_) => {
            ui.label(tl!("Sign in to open and save documents in Nextcloud and to share them."));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(tl!("Server"));
                let r = ui.add(egui::TextEdit::singleline(&mut cf.server).hint_text("cloud.example.com").desired_width(280.0));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, tl!("Nextcloud server address")));
            });
            ui.add_space(8.0);
            let ready = !cf.busy() && !cf.server.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new(tl!("Sign in to Nextcloud…"))).clicked() {
                let server = cf.server.trim().to_string();
                cf.status = Some((tl!("Your browser shows Nextcloud's sign-in page: approve WordLogic there.").to_string(), false));
                ask(app, &mut cf, "account.signIn", json!({"server": server}), Asked::SignIn);
            }
            if cf.signing_in() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tl!("Waiting for you to approve WordLogic in your browser…"));
                });
            }
        }
    }
    status(ui, &cf);
    app.cloud_files = cf;
}

/// How a browser row was clicked.
enum Picked {
    Folder(String),
    File(Item),
}

/// The folder browser shared by Open and Save As. Returns true when the Account page is wanted
/// (not signed in).
fn browser(app: &WordApp, cf: &mut CloudFiles, ui: &mut Ui) -> (bool, Option<Picked>) {
    ensure_account(app, cf);
    ensure_listing(app, cf);
    let t = Tokens::get(ui.ctx());
    let mut picked = None;
    match &cf.account {
        None => {
            ui.spinner();
            return (false, None);
        }
        Some(a) if !a.signed_in => {
            ui.label(RichText::new(tl!("Sign in to Nextcloud to open and save documents there.")).color(t.text_dim));
            let account = ui.link(tl!("Sign in…")).clicked();
            return (account, None);
        }
        Some(_) => {}
    }
    // Where: All files › folder › subfolder, each a link (the section is headed Nextcloud).
    ui.horizontal_wrapped(|ui| {
        if ui.link(tl!("All files")).clicked() {
            picked = Some(Picked::Folder(String::new()));
        }
        let mut path = String::new();
        for part in cf.folder.split('/').filter(|p| !p.is_empty()) {
            ui.label(RichText::new("›").color(t.text_dim));
            path = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            if ui.link(part).clicked() {
                picked = Some(Picked::Folder(path.clone()));
            }
        }
    });
    ui.add_space(6.0);
    match &cf.items {
        None => {
            ui.spinner();
        }
        Some(items) if items.is_empty() => {
            ui.label(RichText::new(tl!("No folders or documents here.")).color(t.text_dim));
        }
        Some(items) => {
            for item in items {
                if row(ui, if item.folder { "folder" } else { "printLayout" }, &item.name, None).clicked() {
                    picked = Some(if item.folder { Picked::Folder(item.path.clone()) } else { Picked::File(item.clone()) });
                }
            }
        }
    }
    (false, picked)
}

/// One row of the browser or of Recent: an icon, a name and, below it, where it is.
pub(crate) fn row(ui: &mut Ui, icon: &str, name: &str, place: Option<&str>) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let h = if place.is_some() { 40.0 } else { 32.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().min(700.0), h), Sense::click());
    // Screen readers hear where it is too.
    let label = match place {
        Some(place) => format!("{name} — {place}"),
        None => name.to_string(),
    };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, t.hover);
    }
    icons::paint(p, Rect::from_center_size(pos2(rect.min.x + 18.0, rect.center().y), vec2(20.0, 20.0)), icon, t.icon, t.accent);
    match place {
        Some(place) => {
            p.text(pos2(rect.min.x + 40.0, rect.min.y + 13.0), Align2::LEFT_CENTER, name, medium(13.0), t.text);
            p.text(pos2(rect.min.x + 40.0, rect.min.y + 29.0), Align2::LEFT_CENTER, place, regular(11.0), t.text_dim);
        }
        None => {
            p.text(pos2(rect.min.x + 40.0, rect.center().y), Align2::LEFT_CENTER, name, medium(13.0), t.text);
        }
    }
    resp
}

/// Opens `remote` (a path in the user's Nextcloud files) from Nextcloud: the host fetches it (or
/// finds the copy here up to date) and the answer opens it.
pub fn open_remote(app: &mut WordApp, remote: &str) {
    let name = remote.rsplit('/').next().unwrap_or(remote).to_string();
    let mut cf = std::mem::take(&mut app.cloud_files);
    let open: Vec<String> = app.session.path.iter().map(|p| p.to_string_lossy().to_string()).collect();
    cf.status = Some((crate::i18n::fmt(tl!("Opening {name}…"), &[("name", &name)]), false));
    ask(app, &mut cf, "files.open", json!({"path": remote, "openPaths": open}), Asked::Open(name));
    app.cloud_files = cf;
}

/// File › Open › Nextcloud. Returns true when the Account page is wanted.
pub fn open_section(app: &mut WordApp, ui: &mut Ui) -> bool {
    section(ui, tl!("Nextcloud"));
    let mut cf = std::mem::take(&mut app.cloud_files);
    let (account, picked) = browser(app, &mut cf, ui);
    let mut open = None;
    match picked {
        Some(Picked::Folder(f)) => go_to(app, &mut cf, &f),
        Some(Picked::File(item)) => open = Some(item.path),
        None => {}
    }
    status(ui, &cf);
    app.cloud_files = cf;
    if let Some(path) = open {
        open_remote(app, &path);
    }
    account
}

/// File › Save As › Nextcloud: pick a folder, type a name, Save. Returns true when the Account
/// page is wanted.
pub fn save_as_section(app: &mut WordApp, ui: &mut Ui) -> bool {
    section(ui, tl!("Nextcloud"));
    let mut cf = std::mem::take(&mut app.cloud_files);
    // The document's own name to start with, once per document (an emptied box stays empty).
    let id = Some(app.session.document_id());
    if cf.named_for != id {
        cf.named_for = id;
        cf.name = app.title_stem();
    }
    let (account, picked) = browser(app, &mut cf, ui);
    match picked {
        Some(Picked::Folder(f)) => go_to(app, &mut cf, &f),
        // Clicking a document takes its name (Save then asks before replacing it).
        Some(Picked::File(item)) => cf.name = item.name,
        None => {}
    }
    if cf.signed_in() {
        ui.add_space(10.0);
        let mut save = false;
        ui.horizontal(|ui| {
            ui.label(tl!("File name"));
            let r = ui.add(egui::TextEdit::singleline(&mut cf.name).desired_width(280.0));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, tl!("Nextcloud file name")));
            if r.changed() {
                cf.replace = None;
            }
            save = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let button = ui.add_enabled(!cf.busy() && !cf.name.trim().is_empty(), egui::Button::new(tl!("Save")));
            // Its name says where (File's own Save is beside it in the navigation).
            let enabled = button.enabled();
            button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tl!("Save to Nextcloud")));
            save |= button.clicked();
        });
        let mut replace = false;
        if let Some(name) = cf.replace.clone() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(crate::i18n::fmt(tl!("{name} is already there. Replace it?"), &[("name", &name)]));
                replace = ui.button(tl!("Replace")).clicked();
                if ui.button(tl!("Cancel")).clicked() {
                    cf.replace = None;
                }
            });
        }
        if (save || replace) && !cf.name.trim().is_empty() {
            let (folder, name) = (cf.folder.clone(), cf.name.trim().to_string());
            cf.replace = None;
            ask(app, &mut cf, "files.saveAs", json!({"folder": folder, "name": name, "replace": replace}), Asked::SaveAs);
        }
    }
    status(ui, &cf);
    app.cloud_files = cf;
    account
}
