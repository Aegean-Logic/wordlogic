//! `@mentions` in comments, kept the way Word keeps them: the `@Name` text is a hyperlink to the
//! person (`mailto:` their address, or their profile page) with the character style `Mention`.
//! Who gets told is the host's business (the Nextcloud app): the comment commands only answer
//! with the people newly mentioned.

use serde_json::{Value, json};
use wordcraft_doc::{Blocks, CharProps, Document, Paragraph, Rgb, StoryRef, Style, StyleKind, TextColor, para_block};

/// The character style of mentions (Word's name for it).
pub const STYLE: &str = "Mention";

/// At most this many people are mentioned in one comment.
const MAX_PEOPLE: usize = 32;

/// Someone to mention: the `mentions` param of the comment commands.
#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    pub name: String,
    /// Their Nextcloud user id, when known (the host notifies by it).
    pub id: String,
    /// Where the mention links: `mailto:…`, or a web page about the person.
    pub link: String,
}

/// A mention in a comment's text: characters `start..start + len` of its plain text.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub start: usize,
    pub len: usize,
    pub name: String,
    pub link: String,
}

/// The `mentions` param: `[{name, id?, link}]`. Links other than `mailto:`, `http:` and `https:`
/// are dropped, as are people without a name.
pub fn people(v: &Value) -> Vec<Person> {
    let Some(list) = v.get("mentions").and_then(Value::as_array) else { return vec![] };
    let text =
        |m: &Value, k: &str, max: usize| m.get(k).and_then(Value::as_str).map(|t| t.trim().chars().take(max).collect::<String>()).unwrap_or_default();
    list.iter()
        .take(MAX_PEOPLE)
        .map(|m| Person { name: text(m, "name", 255), id: text(m, "id", 255), link: text(m, "link", 2048) })
        .filter(|p| !p.name.is_empty() && !p.name.contains('\n'))
        .filter(|p| {
            let l = p.link.to_ascii_lowercase();
            l.starts_with("mailto:") || l.starts_with("https://") || l.starts_with("http://")
        })
        .collect()
}

/// Each person's first `@Name` in `text` (byte ranges, in text order); an `@Ann` that is the
/// start of `@Anna` doesn't count. People whose `@Name` was edited away aren't mentioned.
fn locate<'a>(text: &str, people: &'a [Person]) -> Vec<(usize, usize, &'a Person)> {
    let mut out: Vec<(usize, usize, &Person)> = vec![];
    for p in people {
        if out.iter().any(|(_, _, q)| q.link == p.link) {
            continue;
        }
        let needle = format!("@{}", p.name);
        let mut from = 0;
        while let Some(i) = text.get(from..).and_then(|t| t.find(&needle)).map(|i| i + from) {
            let end = i + needle.len();
            let next = text.get(end..).and_then(|t| t.chars().next());
            let overlaps = out.iter().any(|(a, b, _)| i < *b && *a < end);
            if !overlaps && !next.is_some_and(char::is_alphanumeric) {
                out.push((i, end, p));
                break;
            }
            from = end;
        }
    }
    out.sort_by_key(|(a, _, _)| *a);
    out
}

/// A comment body from `text` (each line a paragraph) with the people's `@Name`s as mentions.
/// Returns the blocks and the people mentioned.
pub fn body(text: &str, people: &[Person]) -> (Blocks, Vec<Person>) {
    let spans = locate(text, people);
    let mention = |link: &str| CharProps { style: Some(STYLE.into()), link: Some(link.to_string()), ..Default::default() };
    let mut blocks = Blocks::new();
    let mut line_start = 0usize;
    for line in text.split('\n').take(10_000) {
        let line_end = line_start + line.len();
        let clean = line.trim_end_matches('\r');
        let mut para = Paragraph::with_text("", CharProps::default());
        let mut pos = line_start;
        for (a, b, p) in spans.iter().filter(|(a, b, _)| *a >= line_start && *b <= line_start + clean.len()) {
            para.append(Paragraph::with_text(text.get(pos..*a).unwrap_or(""), CharProps::default()));
            para.append(Paragraph::with_text(text.get(*a..*b).unwrap_or(""), mention(&p.link)));
            pos = *b;
        }
        para.append(Paragraph::with_text(text.get(pos..line_start + clean.len()).unwrap_or(""), CharProps::default()));
        blocks.push(para_block(para));
        line_start = line_end + 1;
    }
    let used = spans.into_iter().map(|(_, _, p)| p.clone()).collect();
    (blocks, used)
}

/// The mentions of a comment's text (its story `part`), with offsets in characters of its plain
/// text: runs in the `Mention` style that link somewhere. Word's own mentions read the same way.
pub fn found(doc: &Document, part: u32) -> Vec<Found> {
    let story = StoryRef::Part(part);
    let plain = doc.plain_text(story);
    let mut out = vec![];
    let mut chars_before = 0usize;
    let mut joined = String::new();
    for (k, path) in doc.para_paths(story).into_iter().enumerate() {
        let Some(para) = doc.para(story, &path) else { continue };
        if k > 0 {
            joined.push('\n');
            chars_before += 1;
        }
        for (range, props) in para.run_ranges() {
            let piece: String = para.text.get(range.clone()).unwrap_or("").chars().filter(|c| *c != wordcraft_doc::para::OBJ).collect();
            if props.style.as_deref() == Some(STYLE)
                && let Some(link) = props.link.as_deref().filter(|l| !l.is_empty())
                && !piece.is_empty()
            {
                let name = piece.strip_prefix('@').unwrap_or(&piece).to_string();
                out.push(Found { start: chars_before, len: piece.chars().count(), name, link: link.to_string() });
            }
            chars_before += piece.chars().count();
            joined.push_str(&piece);
        }
    }
    // Anything the plain text holds differently (a table in a comment): no mentions rather than
    // misplaced ones.
    if joined != plain {
        return vec![];
    }
    out
}

/// The people in `now` who weren't mentioned `before` (by link): the ones to tell.
pub fn new_people(now: &[Person], before: &[Found]) -> Value {
    json!(
        now.iter()
            .filter(|p| !before.iter().any(|f| f.link.eq_ignore_ascii_case(&p.link)))
            .map(|p| json!({"name": p.name, "id": p.id, "link": p.link}))
            .collect::<Vec<_>>()
    )
}

/// Adds the `Mention` character style when the document doesn't have one (an accent colour on a
/// light tint, so mentions stand out from links).
pub fn ensure_style(doc: &mut Document) {
    if doc.styles.get(STYLE).is_some() {
        return;
    }
    doc.styles.styles.push(Style {
        id: STYLE.into(),
        name: STYLE.into(),
        kind: StyleKind::Character,
        based_on: Some("DefaultParagraphFont".into()),
        chr: CharProps { color: Some(TextColor::Rgb(Rgb(0x1F, 0x5C, 0x99))), shading: Some(Rgb(0xE4, 0xEB, 0xF3)), ..Default::default() },
        priority: Some(99),
        builtin: true,
        ..Default::default()
    });
}

/// A durable id for a new comment (Word's `w16cid:durableId`, kept across saves): links to the
/// comment (a mention's notification) name it. Derived from the comment's id, which is unique
/// among everyone editing, so two people's new comments never share one.
pub fn durable_id(doc: &Document, id: u32) -> String {
    let mut v = 0x2000_0000u32.wrapping_add(id) % 0x7FFF_FFFE + 1;
    loop {
        let d = format!("{v:08X}");
        if !doc.comments.values().any(|c| c.durable_id.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(&d))) {
            return d;
        }
        v = v % 0x7FFF_FFFE + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann() -> Person {
        Person { name: "Ann Lee".into(), id: "ann".into(), link: "mailto:ann@example.com".into() }
    }

    #[test]
    fn mentions_become_styled_links_and_read_back() {
        let mut doc = Document::default();
        let bo = Person { name: "Bø".into(), id: "bo".into(), link: "https://cloud.example/u/bo".into() };
        let (blocks, used) = body("Hi @Ann Lee,\n😀 @Bø and @Ann Lee again", &[ann(), bo.clone()]);
        assert_eq!(used, vec![ann(), bo]);
        let part = doc.try_add_part(wordcraft_doc::PartKind::Comment, blocks).unwrap();
        assert_eq!(doc.plain_text(StoryRef::Part(part)), "Hi @Ann Lee,\n😀 @Bø and @Ann Lee again");
        let f = found(&doc, part);
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].start, f[0].len, f[0].name.as_str()), (3, 8, "Ann Lee"));
        assert_eq!((f[1].start, f[1].len, f[1].link.as_str()), (15, 3, "https://cloud.example/u/bo"));
    }

    #[test]
    fn only_new_people_are_told_and_bad_links_are_refused() {
        let v = json!({"mentions": [
            {"name": "Ann Lee", "id": "ann", "link": "mailto:ann@example.com"},
            {"name": "Evil", "link": "javascript:alert(1)"},
            {"name": "", "link": "mailto:x@example.com"},
        ]});
        let p = people(&v);
        assert_eq!(p, vec![ann()]);
        let before = vec![Found { start: 0, len: 8, name: "Ann Lee".into(), link: "MAILTO:ann@example.com".into() }];
        assert_eq!(new_people(&p, &before), json!([]));
        assert_eq!(new_people(&p, &[]).as_array().map(Vec::len), Some(1));
        // `@Ann` at the start of `@Anna` isn't Ann.
        let (_, used) = body("@Ann Leeds", &p);
        assert!(used.is_empty());
    }

    #[test]
    fn durable_ids_are_valid_and_unique() {
        let mut doc = Document::default();
        let d = durable_id(&doc, 5);
        assert_eq!(d, "20000006");
        doc.comments.insert(1, wordcraft_doc::Comment { durable_id: Some(d.clone()), ..Default::default() });
        assert_ne!(durable_id(&doc, 5), d);
        let top = durable_id(&doc, u32::MAX);
        assert!(u32::from_str_radix(&top, 16).is_ok_and(|v| v != 0 && v < 0x7FFF_FFFF), "{top}");
    }
}
