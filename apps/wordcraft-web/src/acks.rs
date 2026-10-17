//! Save acknowledgements in host mode (`?host=parent&ack=1`), and the checks on the names the
//! page sends.
//!
//! Without acknowledgements, a document handed to the parent (`wordcraft:save`) counts as saved at
//! once, even when the parent then fails to store it. With them, every save carries an `id` and
//! runs as one of the app's saves that finish later ([`AsyncSaves`] tickets, as the browser's save
//! picker does): the parent answers `wordcraft:saved { id }` once it stored the file, or
//! `wordcraft:saveFailed { id, error }`, which leaves the document unsaved and tells the user. A
//! Save Changes prompt's New or Open waits for that answer. Edits made after the handover stay
//! unsaved either way. No browser types here, so it's tested natively.
//!
//! [`AsyncSaves`]: wordcraft_ui_egui::async_saves::AsyncSaves

use wordcraft_ui_egui::async_saves::SaveOutcome;

/// The documents File › Open offers, and the names `wordcraft:open` accepts.
pub const DOC_EXTS: &[&str] = &["docx", "docm", "dotx", "dotm", "doc", "dot", "odt", "rtf", "txt", "md", "html", "htm", "tex", "json", "xml"];
/// The name a document from the page opens under when the page gives none.
pub const DEFAULT_NAME: &str = "Document.docx";

/// Saves still waiting for an answer. Past this the oldest is given up on (as failed), so a parent
/// that never answers can't grow the list without bound.
const MAX_PENDING: usize = 64;
/// The longest file name taken from the parent.
const MAX_NAME_CHARS: usize = 255;
/// The longest error text taken from the parent.
const MAX_ERROR_CHARS: usize = 500;
/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The parent's answer to a `wordcraft:save`.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// Stored, under `name` when the parent stored it under another name (a copy, after a
    /// conflict).
    Saved {
        id: u64,
        name: Option<String>,
    },
    Failed {
        id: u64,
        error: String,
    },
}

impl Reply {
    /// The answer in a message from the parent: its `type`, `id`, and `name` or `error`. `None`
    /// for any other message, or one without a valid `id`.
    pub fn parse(kind: &str, id: Option<f64>, name: Option<String>, error: Option<String>) -> Option<Reply> {
        let id = parse_id(id)?;
        match kind {
            "wordcraft:saved" => Some(Reply::Saved { id, name: name.as_deref().and_then(clean_name) }),
            "wordcraft:saveFailed" => {
                let error: String = error.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(MAX_ERROR_CHARS).collect();
                Some(Reply::Failed { id, error: if error.is_empty() { "the page couldn't store it".into() } else { error } })
            }
            _ => None,
        }
    }
}

struct Pending {
    id: u64,
    /// The name it was handed over as.
    name: String,
    /// The document itself (a format that keeps everything) rather than a copy such as a PDF:
    /// storing it saves the document, under its name.
    document: bool,
    /// The app's ticket for it ([`AsyncSaves::start`](wordcraft_ui_egui::async_saves::AsyncSaves::start)).
    ticket: Option<u64>,
    /// The document it saved (`Session::document_id`), recorded after the frame that saved.
    doc: Option<u64>,
}

#[derive(Default)]
pub struct SaveAcks {
    last_id: u64,
    pending: Vec<Pending>,
    /// Tickets that ended, for the app's [`AsyncSaves`](wordcraft_ui_egui::async_saves::AsyncSaves).
    finished: Vec<(u64, SaveOutcome)>,
}

impl SaveAcks {
    /// `name` is being handed to the parent (`document`: it is the document, not a copy): the id
    /// its answer will carry.
    pub fn begin(&mut self, name: &str, document: bool) -> u64 {
        self.last_id = self.last_id.saturating_add(1);
        if self.pending.len() >= MAX_PENDING {
            let oldest = self.pending.remove(0);
            Self::finish(&mut self.finished, oldest.ticket, SaveOutcome::Failed("the page didn't answer".into()));
        }
        self.pending.push(Pending { id: self.last_id, name: name.to_string(), document, ticket: None, doc: None });
        self.last_id
    }

    /// The save `id` reached the parent, and the app waits on `ticket` for its answer.
    pub fn started(&mut self, id: u64, ticket: u64) {
        match self.pending.iter_mut().find(|p| p.id == id) {
            Some(p) => p.ticket = Some(ticket),
            None => self.finished.push((ticket, SaveOutcome::Failed("the page didn't answer".into()))),
        }
    }

    /// The save `id` never reached the parent.
    pub fn forget(&mut self, id: u64) {
        self.pending.retain(|p| p.id != id);
    }

    /// After the frame that saved: its saves were of `document`, the one open now.
    pub fn bind(&mut self, document: u64) {
        for p in self.pending.iter_mut().filter(|p| p.doc.is_none()) {
            p.doc = Some(document);
        }
    }

    /// Whether a save of `document` is still waiting for the parent: until it answers, the
    /// document isn't safely stored.
    pub fn unconfirmed(&self, document: u64) -> bool {
        self.pending.iter().any(|p| p.document && p.doc.is_none_or(|d| d == document))
    }

    /// Apply the parent's answer. Returns the document a stored save was of and the name it is
    /// now stored under, for the document to take that name.
    pub fn apply(&mut self, reply: Reply) -> Option<(u64, String)> {
        match reply {
            Reply::Saved { id, name } => {
                let index = self.pending.iter().position(|p| p.id == id)?;
                let p = self.pending.remove(index);
                Self::finish(&mut self.finished, p.ticket, SaveOutcome::Saved);
                if !p.document {
                    return None;
                }
                let doc = p.doc?;
                // Earlier saves of the same document are superseded: the page has a newer copy, so
                // a late failure of one of them mustn't mark the document unsaved.
                let (older, rest): (Vec<Pending>, Vec<Pending>) =
                    std::mem::take(&mut self.pending).into_iter().partition(|q| q.id < id && q.document && q.doc == Some(doc));
                self.pending = rest;
                for q in older {
                    Self::finish(&mut self.finished, q.ticket, SaveOutcome::Saved);
                }
                // The page's own name only when it is the same kind of file.
                let name = name.filter(|n| ext(n).eq_ignore_ascii_case(ext(&p.name))).unwrap_or(p.name);
                Some((doc, name))
            }
            Reply::Failed { id, error } => {
                let index = self.pending.iter().position(|p| p.id == id)?;
                let p = self.pending.remove(index);
                Self::finish(&mut self.finished, p.ticket, SaveOutcome::Failed(error));
                None
            }
        }
    }

    /// The tickets that ended since the last call, oldest first.
    pub fn take_finished(&mut self) -> Vec<(u64, SaveOutcome)> {
        std::mem::take(&mut self.finished)
    }

    fn finish(finished: &mut Vec<(u64, SaveOutcome)>, ticket: Option<u64>, outcome: SaveOutcome) {
        if let Some(ticket) = ticket {
            finished.push((ticket, outcome));
        }
    }
}

/// A save id from the parent: a whole, non-negative number a JavaScript number holds exactly.
fn parse_id(id: Option<f64>) -> Option<u64> {
    let id = id?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&id) && id.fract() == 0.0).then_some(id as u64)
}

/// A file name from the parent (or the app): its last path component, trimmed and capped, without
/// control characters. `None` when nothing usable is left.
pub fn clean_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = last.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let name = name.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// The name a document from the page (`wordcraft:open`) opens under: [`DEFAULT_NAME`] when it
/// gives none, `None` when it isn't a document WordCraft opens (a picture or a font would be
/// inserted or installed instead).
pub fn open_name(name: Option<&str>) -> Option<String> {
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return Some(DEFAULT_NAME.to_string());
    };
    let name = clean_name(name)?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.trim().is_empty() && DOC_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))).then_some(name)
}

fn ext(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, e)| e)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save handed over and started, as the host does it, bound to `doc`.
    fn handed(acks: &mut SaveAcks, name: &str, document: bool, ticket: u64, doc: u64) -> u64 {
        let id = acks.begin(name, document);
        acks.started(id, ticket);
        acks.bind(doc);
        id
    }

    #[test]
    fn saved_only_after_the_parent_says_so() {
        let mut acks = SaveAcks::default();
        let id = handed(&mut acks, "Report.docx", true, 10, 1);
        assert!(acks.unconfirmed(1), "handing the document over doesn't store it yet");
        assert!(acks.take_finished().is_empty());
        assert_eq!(acks.apply(Reply::Saved { id, name: None }), Some((1, "Report.docx".into())));
        assert_eq!(acks.take_finished(), vec![(10, SaveOutcome::Saved)]);
        assert!(!acks.unconfirmed(1));
    }

    #[test]
    fn a_failed_save_ends_its_ticket_with_the_page_error() {
        let mut acks = SaveAcks::default();
        let id = handed(&mut acks, "Report.docx", true, 10, 1);
        assert_eq!(acks.apply(Reply::Failed { id, error: "412 Precondition Failed".into() }), None);
        assert_eq!(acks.take_finished(), vec![(10, SaveOutcome::Failed("412 Precondition Failed".into()))]);
        assert!(!acks.unconfirmed(1));
        // A failure for an id we don't know (never sent, or already answered) changes nothing.
        assert_eq!(acks.apply(Reply::Failed { id: 99, error: "offline".into() }), None);
        assert_eq!(acks.apply(Reply::Failed { id, error: "again".into() }), None);
        assert!(acks.take_finished().is_empty());
    }

    #[test]
    fn a_late_failure_of_a_superseded_save_is_ignored() {
        let mut acks = SaveAcks::default();
        let first = handed(&mut acks, "Report.docx", true, 10, 1);
        let second = handed(&mut acks, "Report.docx", true, 11, 1);
        acks.apply(Reply::Saved { id: second, name: None });
        // The older save is settled too: the page holds a newer copy.
        assert_eq!(acks.take_finished(), vec![(11, SaveOutcome::Saved), (10, SaveOutcome::Saved)]);
        assert!(!acks.unconfirmed(1));
        assert_eq!(acks.apply(Reply::Failed { id: first, error: "late".into() }), None);
        assert!(acks.take_finished().is_empty(), "the late failure doesn't mark the document unsaved");
    }

    #[test]
    fn a_newer_save_that_fails_stays_failed_when_an_older_one_lands() {
        let mut acks = SaveAcks::default();
        let first = handed(&mut acks, "Report.docx", true, 10, 1);
        let second = handed(&mut acks, "Report.docx", true, 11, 1);
        acks.apply(Reply::Failed { id: second, error: "disk full".into() });
        assert!(acks.unconfirmed(1), "the older save is still out");
        acks.apply(Reply::Saved { id: first, name: None });
        assert_eq!(acks.take_finished(), vec![(11, SaveOutcome::Failed("disk full".into())), (10, SaveOutcome::Saved)]);
    }

    #[test]
    fn the_page_can_rename_the_document_but_not_its_kind() {
        let mut acks = SaveAcks::default();
        let id = handed(&mut acks, "Report.docx", true, 1, 1);
        assert_eq!(acks.apply(Reply::Saved { id, name: Some("Report (copy).DOCX".into()) }), Some((1, "Report (copy).DOCX".into())));
        let id = handed(&mut acks, "Report.docx", true, 2, 1);
        assert_eq!(acks.apply(Reply::Saved { id, name: Some("Report.pdf".into()) }), Some((1, "Report.docx".into())));
    }

    #[test]
    fn saves_bind_to_the_document_open_when_they_were_made() {
        let mut acks = SaveAcks::default();
        let id = handed(&mut acks, "First.docx", true, 1, 1);
        // Another document opened since: the answer still names the first one, so the host can
        // leave the new one alone.
        acks.bind(2);
        assert!(acks.unconfirmed(1) && !acks.unconfirmed(2));
        assert_eq!(acks.apply(Reply::Saved { id, name: None }), Some((1, "First.docx".into())));
    }

    #[test]
    fn copies_and_unknown_or_unsent_saves_change_nothing() {
        let mut acks = SaveAcks::default();
        let pdf = handed(&mut acks, "Report.pdf", false, 5, 1);
        assert!(!acks.unconfirmed(1), "an export isn't the document");
        assert_eq!(acks.apply(Reply::Saved { id: pdf, name: None }), None);
        assert_eq!(acks.take_finished(), vec![(5, SaveOutcome::Saved)]);
        let unsent = acks.begin("Report.docx", true);
        acks.forget(unsent);
        acks.bind(1);
        assert!(!acks.unconfirmed(1));
        assert_eq!(acks.apply(Reply::Saved { id: unsent, name: None }), None);
        assert_eq!(acks.apply(Reply::Saved { id: 1234, name: None }), None);
        assert!(acks.take_finished().is_empty());
        // A ticket for an id that is no longer waiting ends at once rather than hanging.
        acks.started(4321, 9);
        assert_eq!(acks.take_finished(), vec![(9, SaveOutcome::Failed("the page didn't answer".into()))]);
    }

    #[test]
    fn pending_saves_are_bounded_and_the_dropped_ones_fail() {
        let mut acks = SaveAcks::default();
        let total = MAX_PENDING * 3;
        for ticket in 0..total as u64 {
            handed(&mut acks, "Report.docx", true, ticket, 1);
        }
        assert_eq!(acks.pending.len(), MAX_PENDING);
        let finished = acks.take_finished();
        assert_eq!(finished.len(), total - MAX_PENDING);
        assert!(finished.iter().all(|(_, o)| matches!(o, SaveOutcome::Failed(_))));
    }

    #[test]
    fn tickets_reach_the_app_saves() {
        let saves = wordcraft_ui_egui::async_saves::AsyncSaves::default();
        let mut acks = SaveAcks::default();
        let id = acks.begin("Report.docx", true);
        acks.started(id, saves.start());
        acks.bind(1);
        assert_eq!(saves.pending(), 1, "the app waits for the page");
        acks.apply(Reply::Saved { id, name: None });
        for (ticket, outcome) in acks.take_finished() {
            saves.finish(ticket, outcome);
        }
        assert_eq!(saves.pending(), 0);
    }

    #[test]
    fn replies_from_the_parent_are_checked() {
        let saved = |id: f64| Reply::parse("wordcraft:saved", Some(id), None, None);
        assert_eq!(saved(3.0), Some(Reply::Saved { id: 3, name: None }));
        for bad in [-1.0, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300] {
            assert_eq!(saved(bad), None, "{bad}");
        }
        assert_eq!(Reply::parse("wordcraft:saved", None, None, None), None);
        assert_eq!(Reply::parse("wordcraft:open", Some(1.0), None, None), None);
        assert_eq!(Reply::parse("gridcraft:saved", Some(1.0), None, None), None);
        assert_eq!(
            Reply::parse("wordcraft:saved", Some(1.0), Some(" Documents/Q3/Report.docx ".into()), None),
            Some(Reply::Saved { id: 1, name: Some("Report.docx".into()) })
        );
        assert_eq!(Reply::parse("wordcraft:saved", Some(1.0), Some("a\\b/ ".into()), None), Some(Reply::Saved { id: 1, name: None }));
        let long = "x".repeat(10_000);
        let Some(Reply::Failed { error, .. }) = Reply::parse("wordcraft:saveFailed", Some(1.0), None, Some(long)) else {
            panic!("a failure parses");
        };
        assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
        assert_eq!(
            Reply::parse("wordcraft:saveFailed", Some(1.0), None, Some(" \n ".into())),
            Some(Reply::Failed { id: 1, error: "the page couldn't store it".into() })
        );
    }

    #[test]
    fn names_from_the_page_are_checked() {
        assert_eq!(clean_name("/remote.php/dav/files/u/Report.docx").as_deref(), Some("Report.docx"));
        assert_eq!(clean_name("C:\\x\\..").as_deref(), None);
        assert_eq!(clean_name("a/.").as_deref(), None);
        assert_eq!(clean_name("Re\u{0}po\nrt.docx").as_deref(), Some("Report.docx"));
        assert_eq!(clean_name(&"é".repeat(1000)).map(|n| n.chars().count()), Some(MAX_NAME_CHARS));

        assert_eq!(open_name(None).as_deref(), Some(DEFAULT_NAME));
        assert_eq!(open_name(Some("  ")).as_deref(), Some(DEFAULT_NAME));
        assert_eq!(open_name(Some("Notes/Minutes.ODT")).as_deref(), Some("Minutes.ODT"));
        assert_eq!(open_name(Some("Report.docm")).as_deref(), Some("Report.docm"));
        for bad in ["photo.png", "font.ttf", "people.csv", "Report", ".docx", "..", "Report.docx/"] {
            assert_eq!(open_name(Some(bad)), None, "{bad}");
        }
    }
}
