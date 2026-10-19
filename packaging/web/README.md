# Hosting WordLogic for the web

`wordcraft-web-<version>.zip` (from the GitHub release, or `packaging/web/package.sh`) holds a
static site in `wordcraft-web-<version>/`:

| File | What it is |
|---|---|
| `index.html` | The page. It loads everything through relative URLs. |
| `wordcraft-web-<hash>.js` | wasm-bindgen glue (generated, ES module) |
| `wordcraft-web-<hash>_bg.wasm` | The app, about 13 MB, or 5 MB with compression |
| `_headers`, `.htaccess` | Sample header rules for Netlify/Cloudflare Pages and Apache |

There is no server-side code. Upload the folder's contents anywhere that serves static files.

## Any path works

All URLs in `index.html` are relative (`public_url = "./"` in `apps/wordcraft-web/Trunk.toml`),
so the site works at a domain root (`https://example.com/`), under a prefix
(`https://example.com/tools/wordcraft/`) and from a CDN bucket. The asset names carry a content
hash, so they can be cached forever. Only `index.html` needs revalidation.

## Required server settings

- **MIME type:** serve `.wasm` as `application/wasm`. Browsers refuse to stream-compile it under
  any other type, and the app then loads slowly or not at all. Serve `.js` as `text/javascript`.
  Most hosts already do both. For nginx, check that `mime.types` has `application/wasm wasm;`.
- **Compression:** turn on gzip or Brotli for `.wasm`, `.js` and `.html`. That takes the
  download from about 13 MB to about 5 MB. You can also precompress (`brotli -k *.wasm`) and let
  the server send `Content-Encoding: br`.
- **Caching:** `Cache-Control: public, max-age=31536000, immutable` on the hashed `.wasm` and
  `.js` files, and `no-cache` on `index.html`.
- **HTTPS:** WebGPU (and the clipboard) only work in a secure context, which means `https://`
  or `http://localhost`. Over plain HTTP elsewhere, the app falls back to WebGL2.
- **No special isolation headers:** WordLogic doesn't use `SharedArrayBuffer`, so it doesn't
  need `Cross-Origin-Opener-Policy` or `Cross-Origin-Embedder-Policy`. If your site already sends
  COEP `require-corp`, also send `Cross-Origin-Resource-Policy: same-origin` (or `cross-origin`
  when the files live on a CDN) on the app's files.

nginx example:

```nginx
location /wordcraft/ {
    types { application/wasm wasm; text/javascript js; text/html html; }
    gzip on;
    gzip_types application/wasm text/javascript text/html;
    location ~* \.(wasm|js)$ { add_header Cache-Control "public, max-age=31536000, immutable"; }
    location ~* index\.html$ { add_header Cache-Control "no-cache"; }
}
```

Local test: `python3 -m http.server 8765` inside the folder, then open http://localhost:8765/.

## Embedding in a page (iframe)

```html
<iframe
  src="https://example.com/wordcraft/"
  title="WordLogic word processor"
  style="width: 100%; height: 720px; border: 0;"
  allow="fullscreen; clipboard-read; clipboard-write"
  allowfullscreen>
</iframe>
```

- The app fills the iframe and follows its size, so size the iframe and not the app.
- Keyboard shortcuts go to the iframe after the user clicks into it, as with any embedded app.
- **Cross-origin embeds** work. Preferences are kept in the iframe's `localStorage`. Browsers
  that partition or block third-party storage may forget them between visits, and the app
  then starts with defaults.
- **Sandboxed iframes** need at least
  `sandbox="allow-scripts allow-same-origin allow-downloads allow-popups"`. Without
  `allow-same-origin` there's no storage. Without `allow-downloads`, Save and Export (browser
  downloads) are blocked.
- Don't send `X-Frame-Options: DENY` or a `frame-ancestors` CSP that excludes the embedding page.

## Opening and saving through the page (`?host=parent`)

With `?host=parent` in the iframe `src`, WordLogic opens documents the embedding page sends it
and hands the page every file it saves instead of downloading it. This suits a page with its own
storage, such as a Nextcloud app. The page must be on the same origin as WordLogic: messages
from anywhere else are ignored, and WordLogic only answers its parent window. In this mode
WordLogic starts on a blank document (not the Start screen or a sample), and it doesn't guard
leaving the page itself: the page does that, from `wordcraft:dirty`.

Messages are plain objects passed with `postMessage`, each with a `type`:

| Message | Direction | Fields |
|---|---|---|
| `wordcraft:ready` | to the page | `version`; `ack` (true with `&ack=1`); `collab` (this build can co-edit: its co-editing protocol, `collab::PROTOCOL`, a number to join a session with; earlier builds said `true`). Send the file to open after this. |
| `wordcraft:open` | from the page | `name`, `bytes` (`ArrayBuffer` or `Uint8Array`, up to 200 MB) |
| `wordcraft:save` | to the page | `name`, `bytes` (a transferred `ArrayBuffer`); `id` with `&ack=1`. Sent for Save, Save As, and Export as PDF or picture. |
| `wordcraft:saved` | from the page (`&ack=1`) | `id`; optional `name` when the page stored the file under another name |
| `wordcraft:saveFailed` | from the page (`&ack=1`) | `id`, `error` (shown to the user) |
| `wordcraft:dirty` | to the page | `dirty`: whether there are unsaved changes (with `&ack=1`, including a save the page hasn't confirmed) |
| `wordcraft:failed` | to the page | `error`: the app couldn't start |

Other query flags: `&author=Name` names the user for new comments and tracked changes;
`&desktop=1` offers Open in Desktop App (below).

**Opening.** `name` is the file's name; only its last path component is used. It must end in an
extension WordLogic opens as a document (`.docx`, `.docm`, `.dotx`, `.dotm`, `.doc`, `.dot`,
`.odt`, `.rtf`, `.txt`, `.md`, `.html`, `.htm`, `.tex`, `.json`, `.xml`); anything else is
ignored. Without a `name` the file opens as `Document.docx`. Empty `bytes` open a new blank
document that saves under `name`, so a page can create a file and open it straight away.

**Saving.** Save writes a Word document in its own format (`.docx`, `.docm`, `.dotx`, `.dotm`)
and anything else (`.odt`, `.rtf`, `.doc`, `.txt`, …) as `.docx` under the same name, as the
browser download does. A save of the document (rather than a copy such as a PDF) gives the
document that name, or the page's `name` from `wordcraft:saved` when it has the same extension,
so the next Save goes to the same file.

**Save confirmation (`&ack=1`).** Without it, a document counts as saved as soon as WordLogic
hands it to the page, even if the page then fails to store it. With it, every `wordcraft:save`
carries an `id` and the page answers `wordcraft:saved` or `wordcraft:saveFailed` with that `id`.
Until it answers, the save is in flight: `wordcraft:dirty` stays true, and a New or Open that
asked to save changes first waits. `wordcraft:saveFailed` leaves the document unsaved and shows
the user the page's `error`. Edits made after the handover stay unsaved either way. When the page
confirms a newer save of the same document, older saves still waiting count as stored too. A page
that leaves more than 64 saves unanswered has the oldest counted as failed. A WordLogic that
doesn't know the flag answers `wordcraft:ready` without `ack: true`.

### Co-editing

Several people can edit one document at once (`docs/co-editing.md`). WordLogic does all the
merging; the page connects it to a server that orders everyone's edits into one numbered log
(for Nextcloud, the `wordcraft-nextcloud` app's `/apps/wordcraft/collab/*` endpoints).

| Message | Direction | Fields |
|---|---|---|
| `wordcraft:collab:start` | from the page | `client` (also accepted as `clientId`: this person's id from the server, 1 to 2⁵³−1), `seq`, optional `readOnly` (this person can view but not edit: edits are taken back with a message), `user` and `color` (`#rrggbb`: how the others see this person), and optional `state`: the state snapshot kept with the checkpoint at `seq` (text, from `checkpointData`). Send right after `wordcraft:open`: the document just opened is the shared one as of log position `seq`, or, with `state`, the snapshot is, and replaces the file just opened. |
| `wordcraft:collab:entries` | from the page | `json`: the server's log entries in order, as JSON text: `[{seq, client, client_seq, base, op}]`, `op` exactly as the server received it. An entry whose op doesn't parse becomes a no-op in its place. |
| `wordcraft:collab:op` | to the page | `json`: this person's next edit for the server, `{client, client_seq, base, op}`. The next comes once this one is back in the log, so send them in order and retry a failed one (the server must recognise a repeated `client`/`client_seq`). |
| `wordcraft:collab:checkpoint` | from the page | `id`: asks for the shared document to save to the file. |
| `wordcraft:collab:checkpointData` | to the page | `id`, `seq`, `name`, `bytes` (the document in the opened file's format: .docx, .odt, .rtf…), `state` (text): the document as everyone has it at log position `seq`, without this person's unconfirmed edits, and its exact state snapshot. A saved file doesn't read back with the same part, comment and list numbers, which everyone's edits refer to, so the server keeps `state` with the checkpoint and hands it to people who join from that file (`collab:start`'s `state`). It can be tens of MB: pictures travel inside it. |
| `wordcraft:collab:selection` | to the page | `json`: this person's position, `{anchor, focus}` (each a position: `{story, path, off}`), whenever it changes. The page passes it on as it is. |
| `wordcraft:collab:presence` | from the page | `json`: where the others are, `[{anchor, focus, client, name, color: "#rrggbb"}]` (their `selection` as their WordLogic sent it, with their client id, name and colour). Each person's caret is drawn in their colour with a tag carrying their name, and their selection as a highlight in their colour; between reports their positions move with the text. |
| `wordcraft:collab:stop` | from the page | Ends sharing. |
| `wordcraft:collab:error` | to the page | `error`: this copy can't follow the log (an entry missing or out of order, a snapshot that doesn't read). Rejoin: load the file again and start over. |

The server's side of the contract:
- Positions are gap-free, from 1.
- Joining returns the position the current file was saved at (with the snapshot kept for it, if
  any), plus every entry after it.
- A joiner that loaded the file before a newer save has to load it again.
- Entries older than the oldest `base` anyone may still send must be kept.

While sharing, Undo and Redo work on this person's own edits only, and never take back a later
change of someone else's. While sharing, a Save of the opened file (`wordcraft:save` under its
name) is the page's to turn into a checkpoint. WordLogic handles co-editing messages as they
arrive, even in a hidden tab, which draws no frames.

### Sharing

The Share button opens WordLogic's Share dialog: add people, groups or email addresses with Can
edit or Can view, write a message, Send, Copy link (with the link's permission in plain sight and
link settings) and manage who has access. Someone who opened a shared link sees Copy link for that
link instead. WordLogic draws the dialog. The page does the talking to the file's server (for
Nextcloud, its sharing API; `wordcraft-nextcloud/src/share.js`). The operations are those of
`wordcraft_ui_egui::share::Cloud`: `share.info`, `share.search`, `share.list`, `share.add`,
`share.update`, `share.remove` and `share.link`. Right-clicking Share exports a copy, as Share
does without a page.

| Message | Direction | Fields |
|---|---|---|
| `wordcraft:cloud` | to the page | `id`, `op` (an operation above), `params` (JSON text). |
| `wordcraft:cloud:reply` | from the page | `id`, `ok`, then `json` (the answer as JSON text, at most 4 MB) when `ok`, otherwise `error` (a sentence for the user). |

### Open in Desktop App

With `desktop=1` in the page's URL, the ribbon offers Open in Desktop App beside Share (an icon in
a narrow window). When it's chosen, WordLogic sends `wordcraft:openDesktop` (to the page). The
page opens the file in WordLogic on the computer with a
`wordlogic://open?server=<Nextcloud address>&path=<file>` link.

## Renderer selection and fallback flags

WordLogic renders with wgpu. It uses **WebGPU** when the browser has it and falls back to
**WebGL2** on its own. URL query flags override this, and they work on the iframe `src` too:

| Flag | Effect |
|---|---|
| *(none)* | WebGPU if available, otherwise WebGL2 |
| `?webgl` | Force the WebGL2 backend (useful when a WebGPU driver misbehaves) |

For example: `<iframe src="https://example.com/wordcraft/?webgl" ...>`.

A browser with neither WebGPU nor WebGL2 gets a message in place of the app.
