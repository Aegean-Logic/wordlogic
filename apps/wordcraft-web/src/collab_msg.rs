//! Co-editing messages from the page (`?host=parent`), checked before they reach the session.
//! No browser types here, so it's tested natively. The protocol is in `packaging/web/README.md`
//! (§ Co-editing) and `../wordcraft-nextcloud` (README, § Protocol).

use serde_json::Value;
use wordcraft_doc::Pos;
use wordcraft_engine::collab::{Incoming, Op};

/// The largest log batch, state snapshot or presence list taken in one message (JSON text).
pub const MAX_JSON_BYTES: usize = 64 << 20;
/// The most people drawn in the document, and the most characters of a name.
const MAX_PEOPLE: usize = 64;
const MAX_NAME_CHARS: usize = 60;
/// The deepest position taken (a paragraph in tables nested 20 deep).
const MAX_PATH_LEN: usize = 64;
/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// What the page asks of the shared session.
#[derive(Debug, PartialEq)]
pub enum FromPage {
    /// `wordcraft:collab:start`: the document just opened is the shared one as of log position
    /// `seq` (or, with `state`, the snapshot is: it replaces the file just opened); this person is
    /// `client`, and with `read_only` can't edit. `user` and `color`: how the others see them.
    Start { client: u64, seq: u64, read_only: bool, user: Option<String>, color: Option<[u8; 3]>, state: Option<String> },
    /// `wordcraft:collab:entries {json}`: log entries, in order.
    Entries(Vec<Incoming>),
    /// `wordcraft:collab:checkpoint {id}`: send the shared document to save.
    Checkpoint { id: u64 },
    /// `wordcraft:collab:stop`: the session ended.
    Stop,
    /// `wordcraft:collab:presence {json}`: where the others are.
    Presence(Vec<Someone>),
}

/// The fields of a page message a co-editing message may carry (read from the browser's object).
#[derive(Debug, Default)]
pub struct Fields {
    pub client: Option<f64>,
    /// `clientId`, the same as `client` (either will do).
    pub client_id: Option<f64>,
    pub seq: Option<f64>,
    pub id: Option<f64>,
    pub read_only: Option<bool>,
    pub json: Option<String>,
    pub user: Option<String>,
    pub color: Option<String>,
    pub state: Option<String>,
}

/// Someone else in the session, as the page reports them: their WordLogic's position
/// ([`wordcraft_engine::Session::collab_presence`]) with their client id, name and colour.
#[derive(Clone, Debug, PartialEq)]
pub struct Someone {
    pub client: u64,
    pub name: String,
    pub color: [u8; 3],
    pub anchor: Pos,
    pub focus: Pos,
}

/// The co-editing message `kind` with `f`, if it is one; `Err` for one that's malformed (told
/// back to the page).
pub fn parse(kind: &str, f: Fields) -> Option<Result<FromPage, String>> {
    let need = |v: Option<f64>, what: &str| whole(v).ok_or_else(|| format!("{kind}: `{what}` must be a whole number"));
    Some(match kind {
        "wordcraft:collab:start" => (|| {
            let client = need(f.client.or(f.client_id), "client")?;
            if client == 0 {
                return Err(format!("{kind}: `client` must not be 0"));
            }
            let seq = need(f.seq, "seq")?;
            let state = match f.state {
                Some(s) if s.len() > MAX_JSON_BYTES => return Err(format!("{kind}: `state` is too large")),
                Some(s) if s.trim().is_empty() => None,
                other => other,
            };
            let user = f.user.map(|u| name(&u)).filter(|u| !u.is_empty());
            Ok(FromPage::Start { client, seq, read_only: f.read_only.unwrap_or(false), user, color: f.color.as_deref().and_then(rgb), state })
        })(),
        "wordcraft:collab:entries" => match f.json {
            None => Err(format!("{kind}: `json` is missing")),
            Some(text) if text.len() > MAX_JSON_BYTES => Err(format!("{kind}: more than {MAX_JSON_BYTES} bytes")),
            Some(text) => entries(&text).map(FromPage::Entries).map_err(|e| format!("{kind}: {e}")),
        },
        "wordcraft:collab:checkpoint" => need(f.id, "id").map(|id| FromPage::Checkpoint { id }),
        "wordcraft:collab:stop" => Ok(FromPage::Stop),
        "wordcraft:collab:presence" => match f.json {
            Some(text) if text.len() <= MAX_JSON_BYTES => people(&text).map(FromPage::Presence).map_err(|e| format!("{kind}: {e}")),
            _ => Err(format!("{kind}: `json` is missing or too large")),
        },
        _ => return None,
    })
}

/// Log entries from JSON text. An entry whose op doesn't parse (a bug or a tampered client
/// elsewhere) keeps its place in the log as a no-op: every copy reads it the same way, so they
/// stay together, where refusing the batch would stall them all.
fn entries(text: &str) -> Result<Vec<Incoming>, String> {
    let raw: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    raw.into_iter()
        .map(|v| {
            let num = |key: &str| v.get(key).and_then(Value::as_u64).ok_or_else(|| format!("an entry without `{key}`"));
            let op: Op = v.get("op").cloned().and_then(|o| serde_json::from_value(o).ok()).unwrap_or_default();
            Ok(Incoming { seq: num("seq")?, client: num("client")?, client_seq: num("client_seq")?, base: num("base")?, op })
        })
        .collect()
}

/// The others from JSON text: `[{anchor, focus, client, name, color}]`. Someone whose position
/// doesn't read is left out rather than the whole list refused.
fn people(text: &str) -> Result<Vec<Someone>, String> {
    let raw: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .filter_map(|v| {
            let pos = |key: &str| {
                serde_json::from_value::<Pos>(v.get(key)?.clone()).ok().filter(|p| !p.path.0.is_empty() && p.path.0.len() <= MAX_PATH_LEN)
            };
            Some(Someone {
                client: v.get("client").and_then(Value::as_u64).unwrap_or(0),
                name: name(v.get("name").and_then(Value::as_str).unwrap_or("")),
                color: v.get("color").and_then(Value::as_str).and_then(rgb).unwrap_or([128, 128, 128]),
                anchor: pos("anchor")?,
                focus: pos("focus")?,
            })
        })
        .take(MAX_PEOPLE)
        .collect())
}

/// A name as shown: one line, at most [`MAX_NAME_CHARS`] characters.
fn name(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect()
}

/// `#rrggbb` as RGB.
fn rgb(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    let byte = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
    match (hex.len(), byte(0), byte(2), byte(4)) {
        (6, Some(r), Some(g), Some(b)) => Some([r, g, b]),
        _ => None,
    }
}

fn whole(v: Option<f64>) -> Option<u64> {
    let v = v?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&v) && v.fract() == 0.0).then_some(v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(f: Fields) -> Option<Result<FromPage, String>> {
        parse("wordcraft:collab:start", f)
    }

    #[test]
    fn start_is_checked() {
        assert_eq!(parse("wordcraft:open", Fields::default()), None, "not a co-editing message");
        assert_eq!(
            start(Fields { client: Some(3.0), seq: Some(0.0), ..Default::default() }),
            Some(Ok(FromPage::Start { client: 3, seq: 0, read_only: false, user: None, color: None, state: None }))
        );
        // As the page sends it: clientId too, the person's name and colour, and a snapshot.
        assert_eq!(
            start(Fields {
                client_id: Some(4.0),
                seq: Some(12.0),
                read_only: Some(true),
                user: Some("  Ada Lovelace\n".into()),
                color: Some("#1F6FB2".into()),
                state: Some("{\"doc\":{}}".into()),
                ..Default::default()
            }),
            Some(Ok(FromPage::Start {
                client: 4,
                seq: 12,
                read_only: true,
                user: Some("Ada Lovelace".into()),
                color: Some([0x1F, 0x6F, 0xB2]),
                state: Some("{\"doc\":{}}".into())
            }))
        );
        // An empty snapshot is none; a colour that isn't #rrggbb is left out.
        assert!(matches!(
            start(Fields { client: Some(3.0), seq: Some(0.0), state: Some(String::new()), color: Some("blue".into()), ..Default::default() }),
            Some(Ok(FromPage::Start { state: None, color: None, .. }))
        ));
        assert!(matches!(start(Fields { client: Some(3.5), seq: Some(0.0), ..Default::default() }), Some(Err(_))));
        assert!(matches!(start(Fields { client: Some(0.0), seq: Some(0.0), ..Default::default() }), Some(Err(_))), "0 isn't a client");
        assert!(matches!(start(Fields { client: Some(-1.0), seq: Some(0.0), ..Default::default() }), Some(Err(_))));
        assert!(matches!(start(Fields { client: Some(3.0), ..Default::default() }), Some(Err(_))), "no seq");
        assert_eq!(parse("wordcraft:collab:checkpoint", Fields { id: Some(7.0), ..Default::default() }), Some(Ok(FromPage::Checkpoint { id: 7 })));
        assert!(matches!(parse("wordcraft:collab:checkpoint", Fields::default()), Some(Err(_))));
        assert_eq!(parse("wordcraft:collab:stop", Fields::default()), Some(Ok(FromPage::Stop)));
    }

    #[test]
    fn log_entries_are_checked() {
        let entries = |json: &str| parse("wordcraft:collab:entries", Fields { json: Some(json.into()), ..Default::default() });
        let entry = Incoming { seq: 1, client: 2, client_seq: 1, base: 0, op: Op::default() };
        let json = serde_json::to_string(&vec![entry.clone()]).unwrap_or_default();
        assert_eq!(entries(&json), Some(Ok(FromPage::Entries(vec![entry]))));
        assert!(matches!(entries("[{]"), Some(Err(_))));
        // A garbled op is a no-op in its place, not a stalled session.
        let garbled = r#"[{"seq":4,"client":2,"client_seq":9,"base":3,"op":{"stories":"nonsense"}}]"#;
        match entries(garbled) {
            Some(Ok(FromPage::Entries(e))) => assert!(e.len() == 1 && e[0].seq == 4 && e[0].op.is_empty()),
            other => panic!("{other:?}"),
        }
        assert!(matches!(entries(r#"[{"client":2}]"#), Some(Err(_))), "no seq");
        assert!(matches!(parse("wordcraft:collab:entries", Fields::default()), Some(Err(_))));
    }

    #[test]
    fn the_others_are_checked() {
        let presence = |json: &str| parse("wordcraft:collab:presence", Fields { json: Some(json.into()), ..Default::default() });
        let long = "x".repeat(500);
        let deep = vec![0; 200];
        let people = format!(
            r##"[{{"anchor":{{"story":"body","path":[0],"off":2}},"focus":{{"story":"body","path":[1],"off":0}},"client":3,"name":"{long}","color":"#12718C"}},
               {{"anchor":{{"story":{{"part":7}},"path":[0],"off":0}},"focus":{{"story":{{"part":7}},"path":[0],"off":0}},"client":4,"name":"Ben","color":"blue"}},
               {{"client":5,"name":"No position"}},
               {{"anchor":{{"story":"body","path":{deep:?},"off":0}},"focus":{{"story":"body","path":[0],"off":0}},"client":6}},
               {{"anchor":{{"story":"body","path":[],"off":0}},"focus":{{"story":"body","path":[0],"off":0}},"client":7}}]"##
        );
        match presence(&people) {
            Some(Ok(FromPage::Presence(p))) => {
                assert_eq!(p.len(), 2, "without a position, or a deep or empty path, someone is left out");
                assert_eq!(p[0].name.chars().count(), MAX_NAME_CHARS);
                assert_eq!(p[0].color, [0x12, 0x71, 0x8C]);
                assert_eq!(p[0].anchor, Pos::body(0, 2));
                assert_eq!(p[0].focus, Pos::body(1, 0));
                assert_eq!(p[1].client, 4);
                assert_eq!(p[1].color, [128, 128, 128], "not #rrggbb: grey");
                assert_eq!(p[1].anchor.story, wordcraft_doc::StoryRef::Part(7));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(presence("{}"), Some(Err(_))));
        let many =
            format!("[{}]", vec![r#"{"anchor":{"story":"body","path":[0],"off":0},"focus":{"story":"body","path":[0],"off":0}}"#; 500].join(","));
        assert!(matches!(presence(&many), Some(Ok(FromPage::Presence(p))) if p.len() == MAX_PEOPLE));
    }
}
