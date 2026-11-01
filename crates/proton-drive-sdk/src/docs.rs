use crate::api::docs::{DocumentMetaDto, RecentDocumentDto};

use crate::client::ProtonDriveClient;
use crate::node::file::FileOperations;
use crate::node::{
    Node, NodeUid, PROTON_DOC_MEDIA_TYPE, PROTON_SHEET_MEDIA_TYPE, is_proton_document,
    is_proton_sheet,
};
use crate::pgp::PgpSessionKey;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::generic_array::typenum::U16;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::aes::Aes256;
use aes_gcm::{AesGcm, KeyInit as AesKeyInit};
use hmac::{Hmac, KeyInit as HmacKeyInit, Mac};

use prost::Message;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;
type Aes256Gcm16 = AesGcm<Aes256, U16>;

const HKDF_SALT_SIZE: usize = 32;
const GCM_IV_SIZE: usize = 16;
const CHUNK_SIGNAL: &[u8] = b"update-chunk-header";

#[derive(Clone, PartialEq, Message)]
pub struct DocumentUpdate {
    #[prost(bytes = "vec", tag = "1")]
    pub encrypted_content: Vec<u8>,
    #[prost(int32, tag = "2")]
    pub version: i32,
    #[prost(uint64, tag = "3")]
    pub timestamp: u64,
    #[prost(string, tag = "4")]
    pub author_address: String,
    #[prost(string, tag = "5")]
    pub uuid: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DocumentUpdateArray {
    #[prost(message, repeated, tag = "1")]
    pub document_updates: Vec<DocumentUpdate>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Commit {
    #[prost(message, tag = "1")]
    pub updates: Option<DocumentUpdateArray>,
    #[prost(int32, tag = "2")]
    pub version: i32,
    #[prost(string, tag = "3")]
    pub lock_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SquashLock {
    #[prost(string, tag = "1")]
    pub lock_id: String,
    #[prost(uint64, tag = "2")]
    pub lock_expiration: u64,
    #[prost(string, tag = "3")]
    pub commit_id: String,
    #[prost(message, tag = "4")]
    pub commit: Option<Commit>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SquashCommit {
    #[prost(string, tag = "1")]
    pub lock_id: String,
    #[prost(string, tag = "2")]
    pub commit_id: String,
    #[prost(message, tag = "3")]
    pub commit: Option<Commit>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SignedPlaintextContent {
    #[prost(bytes = "vec", tag = "1")]
    pub content: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Doc,
    Sheet,
}

impl DocumentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Sheet => "sheet",
        }
    }

    fn from_media_type(media_type: Option<&str>) -> Option<Self> {
        if is_proton_document(media_type) {
            Some(Self::Doc)
        } else if is_proton_sheet(media_type) {
            Some(Self::Sheet)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub enum DocsTarget {
    Private {
        uid: NodeUid,
    },
    Public {
        token: String,
        link_id: String,
        password: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct DecryptedUpdate {
    pub content: Vec<u8>,
    pub signature: Vec<u8>,
    pub author_address: String,
    pub timestamp: u64,
    pub uuid: String,
}

#[derive(Debug, Clone)]
pub struct OpenedDocument {
    pub uid: NodeUid,
    pub name: String,
    pub kind: DocumentKind,
    pub meta: DocumentMetaDto,
    pub commit_id: Option<String>,
    pub updates: Vec<DecryptedUpdate>,
}

impl OpenedDocument {
    pub fn yjs_bytes(&self) -> usize {
        self.updates.iter().map(|u| u.content.len()).sum()
    }

    pub fn to_markdown(&self) -> anyhow::Result<String> {
        updates_to_markdown(&self.updates)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DdocPublish {
    #[serde(rename = "ddocId")]
    pub ddoc_id: String,
    pub title: Option<String>,
    #[serde(rename = "syncStatus")]
    pub sync_status: Option<String>,
    pub link: Option<String>,
}

impl DdocPublish {
    pub fn open_url(&self) -> String {
        self.owner_url()
    }

    /// Logged-in owner URL. The `#key` share link is guest comment/suggest.
    pub fn owner_url(&self) -> String {
        if let Some(link) = &self.link {
            if let Ok(mut parsed) = reqwest::Url::parse(link) {
                parsed.set_fragment(None);
                return parsed.to_string();
            }
        }
        format!("https://ddocs.new/d/{}", self.ddoc_id)
    }

    pub fn share_url(&self) -> Option<&str> {
        self.link.as_deref()
    }
}

pub fn strip_duplicate_title(markdown: &str, title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return markdown.to_string();
    }
    let mut lines = markdown.lines();
    let Some(first) = lines.next() else {
        return markdown.to_string();
    };
    let heading = first.trim().trim_start_matches('#').trim();
    if !heading.eq_ignore_ascii_case(title) {
        return markdown.to_string();
    }
    lines
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSyncTick {
    Changed,
    Write,
    Idle,
}

/// Debounce ddocs→Proton writes: emit `Write` after content is unchanged for one extra poll.
pub fn live_sync_tick(
    current: &str,
    last_seen: &mut String,
    last_written: &str,
    stable: &mut u32,
) -> LiveSyncTick {
    if current == last_seen.as_str() {
        *stable = stable.saturating_add(1);
        if current != last_written && *stable >= 2 {
            return LiveSyncTick::Write;
        }
        LiveSyncTick::Idle
    } else {
        *last_seen = current.to_string();
        *stable = 0;
        LiveSyncTick::Changed
    }
}

#[derive(serde::Deserialize)]
struct DdocCreateResponse {
    data: DdocPublish,
}

fn apply_updates(updates: &[DecryptedUpdate]) -> (yrs::Doc, usize) {
    use yrs::Transact;

    let doc = yrs::Doc::new();
    let mut applied = 0usize;
    {
        let mut txn = doc.transact_mut();
        for update in updates {
            if let Some(decoded) = yjs_update(&update.content) {
                if txn.apply_update(decoded).is_ok() {
                    applied += 1;
                }
            }
        }
    }
    (doc, applied)
}

pub fn updates_to_markdown(updates: &[DecryptedUpdate]) -> anyhow::Result<String> {
    use yrs::{GetString, ReadTxn, Transact};

    // Proton Docs is a Lexical editor; its own layout is read first, on a
    // separate copy because reading it fixes the type of the `root` entry.
    let markdown = lexical_markdown(&apply_updates(updates).0);
    if !markdown.trim().is_empty() {
        return Ok(markdown.trim().to_string());
    }

    let (doc, applied) = apply_updates(updates);
    {
        let txn = doc.transact();
        if txn
            .root_refs()
            .any(|(name, _)| name == "sheetDataV2" || name == "sheets")
        {
            anyhow::bail!("exporting Proton Sheets spreadsheets is not supported");
        }
    }

    let txn = doc.transact();
    let mut chunks = Vec::new();
    for (_name, value) in txn.root_refs() {
        let s = value.to_string(&txn);
        if !s.trim().is_empty() && s != "{}" && s != "[]" && s != "null" {
            chunks.push(xml_to_markdown(&s));
        }
    }

    for name in [
        "default",
        "prosemirror",
        "content",
        "doc",
        "root",
        "article",
        "editor",
    ] {
        if let Some(frag) = txn.get_xml_fragment(name) {
            let xml = frag.get_string(&txn);
            if !xml.trim().is_empty() {
                chunks.push(xml_to_markdown(&xml));
            }
        }
        if let Some(text) = txn.get_text(name) {
            let s = text.get_string(&txn);
            if !s.trim().is_empty() {
                chunks.push(s);
            }
        }
    }

    let mut md = unique_chunks(chunks);
    if md.trim().is_empty() {
        let mut raw = String::new();
        for update in updates {
            if let Some(json) = json_text(&update.content) {
                raw.push_str(&json);
                raw.push('\n');
            } else {
                raw.push_str(&utf8_runs(&update.content));
                raw.push('\n');
            }
        }
        md = raw;
    }

    let md = md.trim().to_string();
    if md.is_empty() {
        let roots: Vec<String> = txn.root_refs().map(|(n, _)| n.to_string()).collect();
        anyhow::bail!(
            "no readable Proton Doc content in yjs updates (applied={applied}, roots={roots:?})"
        );
    }
    Ok(md)
}

/// Converts a document stored by Lexical's Yjs binding: `root` is an XmlText
/// whose embedded XmlTexts are block elements (their attributes say which), and
/// each run of text is preceded by a Map embed holding the text node's format.
fn lexical_markdown(doc: &yrs::Doc) -> String {
    use yrs::Transact;
    use yrs::types::text::YChange;
    use yrs::{Out, Text};

    let root = doc.get_or_insert_text("root");
    let txn = doc.transact();
    let mut blocks = Vec::new();
    for chunk in root.diff(&txn, YChange::identity) {
        if let Out::YXmlText(block) = chunk.insert {
            lexical_block(&txn, &block, 0, &mut blocks);
        }
    }
    blocks.join("\n\n")
}

fn lexical_attr<T: yrs::ReadTxn>(txn: &T, element: &yrs::XmlTextRef, name: &str) -> String {
    use yrs::Xml;
    match element.get_attribute(txn, name) {
        Some(yrs::Out::Any(yrs::Any::String(value))) => value.to_string(),
        Some(yrs::Out::Any(yrs::Any::Number(value))) => lexical_number(value).to_string(),
        Some(yrs::Out::Any(yrs::Any::Bool(value))) => value.to_string(),
        _ => String::new(),
    }
}

fn lexical_number(value: yrs::any::Number) -> i64 {
    match value {
        yrs::any::Number::Int(value) => value,
        yrs::any::Number::Float(value) => value as i64,
    }
}

fn lexical_block<T: yrs::ReadTxn>(
    txn: &T,
    block: &yrs::XmlTextRef,
    depth: usize,
    out: &mut Vec<String>,
) {
    let kind = lexical_attr(txn, block, "__type");
    match kind.as_str() {
        "list" => {
            let list_type = lexical_attr(txn, block, "__listType");
            let mut items = Vec::new();
            let mut number = lexical_attr(txn, block, "__start").parse().unwrap_or(1u64);
            for child in lexical_children(txn, block) {
                let checked = lexical_attr(txn, &child, "__checked") == "true";
                let marker = match list_type.as_str() {
                    "number" => {
                        number += 1;
                        format!("{}.", number - 1)
                    }
                    "check" if checked => "- [x]".to_string(),
                    "check" => "- [ ]".to_string(),
                    _ => "-".to_string(),
                };
                let mut nested = Vec::new();
                let text = lexical_inline(txn, &child, &mut |list| {
                    lexical_block(txn, list, depth + 1, &mut nested)
                });
                let indent = "  ".repeat(depth);
                if !text.is_empty() || nested.is_empty() {
                    items.push(format!("{indent}{marker} {text}"));
                }
                items.extend(nested);
            }
            out.push(items.join("\n"));
        }
        "table" => {
            let mut rows = Vec::new();
            for (index, row) in lexical_children(txn, block).into_iter().enumerate() {
                let cells: Vec<String> = lexical_children(txn, &row)
                    .iter()
                    .map(|cell| {
                        let mut parts = Vec::new();
                        for paragraph in lexical_children(txn, cell) {
                            lexical_block(txn, &paragraph, 0, &mut parts);
                        }
                        parts.join(" ").replace('|', "\\|").replace('\n', " ")
                    })
                    .collect();
                rows.push(format!("| {} |", cells.join(" | ")));
                if index == 0 {
                    rows.push(format!("|{}", " --- |".repeat(cells.len())));
                }
            }
            out.push(rows.join("\n"));
        }
        "horizontalrule" => out.push("---".into()),
        "code" => {
            let language = lexical_attr(txn, block, "__language");
            let text = lexical_plain(txn, block);
            out.push(format!("```{language}\n{text}\n```"));
        }
        _ => {
            let mut nested = Vec::new();
            let text = lexical_inline(txn, block, &mut |child| {
                lexical_block(txn, child, depth, &mut nested)
            });
            let prefix = match kind.as_str() {
                "heading" => {
                    let level = lexical_attr(txn, block, "__tag")
                        .trim_start_matches('h')
                        .parse()
                        .unwrap_or(1usize);
                    format!("{} ", "#".repeat(level.clamp(1, 6)))
                }
                "quote" => "> ".into(),
                _ => String::new(),
            };
            if !text.is_empty() {
                out.push(format!("{prefix}{text}"));
            }
            out.extend(nested);
        }
    }
}

fn lexical_children<T: yrs::ReadTxn>(txn: &T, element: &yrs::XmlTextRef) -> Vec<yrs::XmlTextRef> {
    use yrs::Text;
    use yrs::types::text::YChange;
    element
        .diff(txn, YChange::identity)
        .into_iter()
        .filter_map(|chunk| match chunk.insert {
            yrs::Out::YXmlText(child) => Some(child),
            _ => None,
        })
        .collect()
}

/// An element's text with formatting; links render inline, and any other
/// nested element (a sublist inside a list item) is passed to `block`.
fn lexical_inline<T: yrs::ReadTxn>(
    txn: &T,
    element: &yrs::XmlTextRef,
    block: &mut dyn FnMut(&yrs::XmlTextRef),
) -> String {
    use yrs::types::text::YChange;
    use yrs::{Map, Out, Text};

    let mut text = String::new();
    let mut format = 0i64;
    for chunk in element.diff(txn, YChange::identity) {
        match chunk.insert {
            Out::YMap(node) => {
                let field = |name: &str| node.get(txn, name);
                match field("__type") {
                    Some(Out::Any(yrs::Any::String(kind))) if &*kind == "linebreak" => {
                        text.push_str("  \n");
                        format = 0;
                    }
                    _ => {
                        format = match field("__format") {
                            Some(Out::Any(yrs::Any::Number(value))) => lexical_number(value),
                            _ => 0,
                        };
                    }
                }
            }
            Out::Any(yrs::Any::String(run)) => text.push_str(&lexical_format(&run, format)),
            Out::YXmlText(child) => match lexical_attr(txn, &child, "__type").as_str() {
                "link" | "autolink" => {
                    let url = lexical_attr(txn, &child, "__url");
                    let label = lexical_inline(txn, &child, block);
                    text.push_str(&format!("[{label}]({url})"));
                }
                _ => block(&child),
            },
            _ => {}
        }
    }
    text
}

fn lexical_plain<T: yrs::ReadTxn>(txn: &T, element: &yrs::XmlTextRef) -> String {
    use yrs::types::text::YChange;
    use yrs::{Map, Out, Text};
    let mut text = String::new();
    for chunk in element.diff(txn, YChange::identity) {
        match chunk.insert {
            Out::Any(yrs::Any::String(run)) => text.push_str(&run),
            Out::YMap(node) => {
                if matches!(node.get(txn, "__type"), Some(Out::Any(yrs::Any::String(kind))) if &*kind == "linebreak")
                {
                    text.push('\n');
                }
            }
            _ => {}
        }
    }
    text
}

/// Lexical's text format bits: 1 bold, 2 italic, 4 strikethrough, 16 code.
fn lexical_format(run: &str, format: i64) -> String {
    if format == 0 || run.trim().is_empty() {
        return run.to_string();
    }
    let mut marked = run.trim().to_string();
    if format & 16 != 0 {
        marked = format!("`{marked}`");
    }
    if format & 4 != 0 {
        marked = format!("~~{marked}~~");
    }
    if format & 2 != 0 {
        marked = format!("*{marked}*");
    }
    if format & 1 != 0 {
        marked = format!("**{marked}**");
    }
    // Markdown emphasis cannot open or close on whitespace, so it stays outside.
    let leading = &run[..run.len() - run.trim_start().len()];
    let trailing = &run[run.trim_end().len()..];
    format!("{leading}{marked}{trailing}")
}

/// The Yjs update inside a decrypted Docs update. Newer clients gzip it, and
/// some updates arrive in a y-protocols sync message (`0`, step `1`/`2`, a
/// length, then the update). A bare update with one client also starts with
/// `0x01`, so the bare form is tried before any envelope.
fn yjs_update(raw: &[u8]) -> Option<yrs::Update> {
    use std::io::Read;
    use yrs::updates::decoder::Decode;

    let inflated;
    let raw = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(raw)
            .read_to_end(&mut out)
            .ok()?;
        inflated = out;
        &inflated[..]
    } else {
        raw
    };
    let decode = |bytes: &[u8]| {
        yrs::Update::decode_v1(bytes)
            .or_else(|_| yrs::Update::decode_v2(bytes))
            .ok()
    };
    if let Some(update) = decode(raw) {
        return Some(update);
    }
    if raw.len() > 2 && raw[0] == 0 && matches!(raw[1], 1 | 2) {
        let mut rest = &raw[2..];
        let mut length = 0usize;
        for shift in (0..64).step_by(7) {
            let (&byte, tail) = rest.split_first()?;
            rest = tail;
            length |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        return decode(rest.get(..length)?);
    }
    None
}

fn unique_chunks(chunks: Vec<String>) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = String::new();
    for chunk in chunks {
        let trimmed = chunk.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(trimmed);
    }
    out
}

fn json_text(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let mut out = String::new();
    collect_json_strings(&v, &mut out);
    let out = out.trim().to_string();
    if out.is_empty() { None } else { Some(out) }
}

fn collect_json_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.len() >= 2 && s.chars().any(|c| c.is_alphanumeric()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_json_strings(x, out)),
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                if matches!(k.as_str(), "text" | "title" | "content" | "markdown") {
                    collect_json_strings(x, out);
                }
            }
            for (k, x) in m {
                if !matches!(k.as_str(), "text" | "title" | "content" | "markdown") {
                    collect_json_strings(x, out);
                }
            }
        }
        _ => {}
    }
}

fn utf8_runs(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(rest) => {
                for c in rest.chars() {
                    if c.is_control() && c != '\n' && c != '\t' {
                        flush_run(&mut cur, &mut out);
                    } else {
                        cur.push(c);
                    }
                }
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    if let Ok(s) = std::str::from_utf8(&bytes[i..i + valid]) {
                        for c in s.chars() {
                            if c.is_control() && c != '\n' && c != '\t' {
                                flush_run(&mut cur, &mut out);
                            } else {
                                cur.push(c);
                            }
                        }
                    }
                    i += valid;
                    continue;
                }
                i += e.error_len().unwrap_or(1);
                flush_run(&mut cur, &mut out);
            }
        }
    }
    flush_run(&mut cur, &mut out);
    out
}

fn flush_run(cur: &mut String, out: &mut String) {
    let t = cur.trim();
    if t.chars().count() >= 4 && t.chars().any(|c| c.is_alphanumeric()) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(t);
    }
    cur.clear();
}

fn xml_to_markdown(xml: &str) -> String {
    let mut out = xml.to_string();
    for (pat, repl) in [
        ("<heading level=\"1\">", "# "),
        ("<heading level=\"2\">", "## "),
        ("<heading level=\"3\">", "### "),
        ("<heading>", "# "),
        ("</heading>", "\n\n"),
        ("<paragraph>", ""),
        ("</paragraph>", "\n\n"),
        ("<listItem>", "- "),
        ("</listItem>", "\n"),
        ("<hardBreak></hardBreak>", "\n"),
        ("<hardBreak/>", "\n"),
        ("<bold>", "**"),
        ("</bold>", "**"),
        ("<italic>", "_"),
        ("</italic>", "_"),
        ("<code>", "`"),
        ("</code>", "`"),
    ] {
        out = out.replace(pat, repl);
    }
    let mut cleaned = String::with_capacity(out.len());
    let mut in_tag = false;
    for c in out.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => cleaned.push(c),
            _ => {}
        }
    }
    let mut lines: Vec<&str> = cleaned.lines().map(str::trim_end).collect();
    while matches!(lines.last(), Some(l) if l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

pub async fn publish_to_ddocs(
    api_url: &str,
    api_key: &str,
    title: &str,
    markdown: &str,
) -> anyhow::Result<DdocPublish> {
    let base = api_url.trim_end_matches('/');
    let url = format!("{base}/api/ddocs?apiKey={api_key}");
    let body_md = {
        let stripped = strip_duplicate_title(markdown, title);
        if stripped.trim().is_empty() {
            String::from("\n")
        } else {
            stripped
        }
    };

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "title": title,
            "content": body_md,
            "fileContent": body_md,
        }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("ddocs API {status}: {body}");
    }
    if let Ok(wrapped) = serde_json::from_str::<DdocCreateResponse>(&body) {
        return Ok(wrapped.data);
    }
    serde_json::from_str::<DdocPublish>(&body)
        .map_err(|e| anyhow::anyhow!("ddocs response: {e}. body: {body}"))
}

pub fn parse_docs_target(input: &str) -> anyhow::Result<DocsTarget> {
    let trimmed = input.trim();
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let link_id = url
            .query_pairs()
            .find(|(k, _)| k == "linkId")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| anyhow::anyhow!("docs URL missing linkId"))?;
        if let Some((_, token)) = url.query_pairs().find(|(k, _)| k == "token") {
            let password = url.fragment().filter(|s| !s.is_empty()).map(str::to_string);
            return Ok(DocsTarget::Public {
                token: token.into_owned(),
                link_id,
                password,
            });
        }

        let volume_id = url
            .query_pairs()
            .find(|(k, _)| k == "volumeId")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| anyhow::anyhow!("docs URL missing volumeId"))?;
        return Ok(DocsTarget::Private {
            uid: NodeUid::from_parts(volume_id, link_id),
        });
    }
    NodeUid::parse(trimmed)
        .map(|uid| DocsTarget::Private { uid })
        .map_err(|e| anyhow::anyhow!(e))
}

pub fn decrypt_commit(
    raw: &[u8],
    content_key: &PgpSessionKey,
) -> anyhow::Result<Vec<DecryptedUpdate>> {
    let commit = Commit::decode(raw)?;
    let updates = commit
        .updates
        .map(|u| u.document_updates)
        .unwrap_or_default();
    let merged = merge_chunked_updates(updates);
    merged
        .into_iter()
        .map(|update| decrypt_update(&update, content_key))
        .collect()
}

fn decrypt_update(
    update: &DocumentUpdate,
    content_key: &PgpSessionKey,
) -> anyhow::Result<DecryptedUpdate> {
    let aad = associated_data(update);
    let plaintext = decrypt_docs_message(&update.encrypted_content, content_key, aad.as_bytes())?;
    let signed = SignedPlaintextContent::decode(plaintext.as_slice())?;
    Ok(DecryptedUpdate {
        content: signed.content,
        signature: signed.signature,
        author_address: update.author_address.clone(),
        timestamp: update.timestamp,
        uuid: update.uuid.clone(),
    })
}

fn associated_data(update: &DocumentUpdate) -> String {
    let author = if update.author_address.is_empty() {
        "anonymous"
    } else {
        update.author_address.as_str()
    };
    format!(
        "docs.rts.{}.{}.{}",
        update.version, author, update.timestamp
    )
}

fn decrypt_docs_message(
    encrypted: &[u8],
    content_key: &PgpSessionKey,
    aad: &[u8],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        encrypted.len() > HKDF_SALT_SIZE + GCM_IV_SIZE,
        "docs ciphertext too short"
    );
    let salt = &encrypted[..HKDF_SALT_SIZE];
    let rest = &encrypted[HKDF_SALT_SIZE..];
    let iv = &rest[..GCM_IV_SIZE];
    let ciphertext = &rest[GCM_IV_SIZE..];
    let key = hkdf_sha256(salt, &content_key.key, aad, 32)?;
    let cipher = <Aes256Gcm16 as AesKeyInit>::new_from_slice(&key)?;

    let nonce = GenericArray::<u8, U16>::from_slice(iv);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("docs AES-GCM decrypt failed: {e}"))
}

fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], length: usize) -> anyhow::Result<Vec<u8>> {
    const HASH_LEN: usize = 32;
    let mut mac = <HmacSha256 as HmacKeyInit>::new_from_slice(salt)
        .map_err(|_| anyhow::anyhow!("HKDF extract failed"))?;

    mac.update(ikm);
    let prk = mac.finalize().into_bytes();

    let n = length.div_ceil(HASH_LEN);
    anyhow::ensure!(n <= 255, "HKDF output too long");

    let mut okm = Vec::with_capacity(n * HASH_LEN);
    let mut t = Vec::new();
    for i in 1..=(n as u8) {
        let mut mac = <HmacSha256 as HmacKeyInit>::new_from_slice(&prk)
            .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?;

        mac.update(&t);
        mac.update(info);
        mac.update(&[i]);
        t = mac.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&t);
    }
    okm.truncate(length);
    Ok(okm)
}

fn encrypt_docs_message(
    plaintext: &[u8],
    content_key: &PgpSessionKey,
    aad: &[u8],
) -> anyhow::Result<Vec<u8>> {
    use rand::Rng;
    let mut salt = vec![0u8; HKDF_SALT_SIZE];
    let mut iv = vec![0u8; GCM_IV_SIZE];
    rand::rng().fill_bytes(&mut salt);
    rand::rng().fill_bytes(&mut iv);
    let key = hkdf_sha256(&salt, &content_key.key, aad, 32)?;
    let cipher = <Aes256Gcm16 as AesKeyInit>::new_from_slice(&key)?;
    let nonce = GenericArray::<u8, U16>::from_slice(&iv);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("docs AES-GCM encrypt failed: {e}"))?;
    let mut out = salt;
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn markdown_to_yjs(markdown: &str) -> anyhow::Result<Vec<u8>> {
    use yrs::{ReadTxn, Text, Transact, WriteTxn, XmlFragment, XmlTextPrelim};

    let doc = yrs::Doc::new();
    {
        let mut txn = doc.transact_mut();
        let frag = txn.get_or_insert_xml_fragment("default");
        let text = txn.get_or_insert_text("default");
        text.insert(&mut txn, 0, markdown);
        // ponytail: one XmlText node, not a full Tiptap schema. Proton still loads the Y.Text fallback; upgrade if the editor ignores it.
        frag.insert(&mut txn, 0, XmlTextPrelim::new(markdown));
    }
    Ok(doc
        .transact()
        .encode_state_as_update_v1(&yrs::StateVector::default()))
}

fn encrypt_yjs_commit(
    yjs: &[u8],
    content_key: &PgpSessionKey,
    author_address: &str,
) -> anyhow::Result<Commit> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let update = DocumentUpdate {
        encrypted_content: Vec::new(),
        version: 1,
        timestamp,
        author_address: author_address.to_string(),
        uuid: format!("{timestamp:032x}"),
    };

    let aad = associated_data(&update);
    let signed = SignedPlaintextContent {
        content: yjs.to_vec(),
        signature: Vec::new(),
    };
    let encrypted = encrypt_docs_message(&signed.encode_to_vec(), content_key, aad.as_bytes())?;
    Ok(Commit {
        updates: Some(DocumentUpdateArray {
            document_updates: vec![DocumentUpdate {
                encrypted_content: encrypted,
                ..update
            }],
        }),
        version: 1,
        lock_id: String::new(),
    })
}

pub async fn fetch_ddoc(
    api_url: &str,
    api_key: &str,
    ddoc_id: &str,
) -> anyhow::Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct DdocGet {
        title: Option<String>,
        content: Option<String>,
        #[serde(rename = "fileContent")]
        file_content: Option<String>,
        data: Option<DdocBody>,
    }
    #[derive(serde::Deserialize)]
    struct DdocBody {
        title: Option<String>,
        content: Option<String>,
        #[serde(rename = "fileContent")]
        file_content: Option<String>,
    }
    let base = api_url.trim_end_matches('/');
    let url = format!("{base}/api/ddocs/{ddoc_id}?apiKey={api_key}");
    let resp = reqwest::Client::new().get(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("ddocs API {status}: {body}");
    }
    let parsed: DdocGet =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("ddocs get: {e}. body: {body}"))?;
    let title = parsed
        .title
        .or_else(|| parsed.data.as_ref().and_then(|d| d.title.clone()))
        .unwrap_or_else(|| "Untitled".into());
    let content = parsed
        .file_content
        .or(parsed.content)
        .or_else(|| parsed.data.and_then(|d| d.file_content.or(d.content)))
        .ok_or_else(|| anyhow::anyhow!("ddocs document has no content"))?;
    Ok((title, content))
}

fn merge_chunked_updates(updates: Vec<DocumentUpdate>) -> Vec<DocumentUpdate> {
    let header_len = CHUNK_SIGNAL.len() + 4;
    let mut pending: std::collections::BTreeMap<u8, (u8, std::collections::BTreeMap<u8, Vec<u8>>)> =
        std::collections::BTreeMap::new();
    let mut out = Vec::new();

    for update in updates {
        if update.encrypted_content.len() < header_len
            || !update.encrypted_content.starts_with(CHUNK_SIGNAL)
        {
            out.push(update);
            continue;
        }
        let data = &update.encrypted_content[CHUNK_SIGNAL.len()..];
        let version = data[0];
        let id = data[1];
        let total = data[2];
        let index = data[3];
        if version != 0 {
            continue;
        }
        let content = update.encrypted_content[header_len..].to_vec();
        let entry = pending
            .entry(id)
            .or_insert_with(|| (total, Default::default()));
        entry.1.insert(index, content);
        if entry.1.len() as u8 == entry.0 {
            let mut merged = Vec::new();
            for i in 0..entry.0 {
                if let Some(chunk) = entry.1.get(&i) {
                    merged.extend_from_slice(chunk);
                }
            }
            pending.remove(&id);
            out.push(DocumentUpdate {
                encrypted_content: merged,
                version: update.version,
                timestamp: update.timestamp,
                author_address: update.author_address,
                uuid: update.uuid,
            });
        }
    }
    out
}

impl ProtonDriveClient {
    pub async fn open_document(&self, target: &str) -> anyhow::Result<OpenedDocument> {
        self.open_document_with_password(target, None).await
    }

    pub async fn open_document_with_password(
        &self,
        target: &str,
        password: Option<&str>,
    ) -> anyhow::Result<OpenedDocument> {
        match parse_docs_target(target)? {
            DocsTarget::Private { uid } => self.open_document_uid(uid).await,
            DocsTarget::Public {
                token,
                link_id,
                password: url_password,
            } => {
                let password = password.or(url_password.as_deref());
                self.open_public_document(&token, &link_id, password).await
            }
        }
    }

    async fn open_public_document(
        &self,
        token: &str,
        link_id: &str,
        password: Option<&str>,
    ) -> anyhow::Result<OpenedDocument> {
        let password = password.filter(|p| !p.is_empty()).ok_or_else(|| {
            anyhow::anyhow!("public docs URL needs a share password (#fragment or --password)")
        })?;
        let drive_url = format!("https://drive.proton.me/urls/{token}#{password}");
        let public =
            crate::sharing::SharingOperations::authenticate_public_link(self, &drive_url).await?;
        let uid = NodeUid::from_parts(public.session().root_uid.volume_id.raw(), link_id);
        let node = match public.get_node(uid.clone()).await {
            Ok(n) => n,
            Err(_) => public.get_root_node().await?,
        }
        .result()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let (name, media_type) = match &node {
            Node::File(f) | Node::Photo(f) => {
                (f.base.base.name.clone(), Some(f.base.media_type.as_str()))
            }
            Node::Folder(_) | Node::Album(_) => (node.base().name.clone(), None),
        };
        let kind = DocumentKind::from_media_type(media_type).unwrap_or(DocumentKind::Doc);
        let content_key = FileOperations::get_secrets(public.drive(), node.uid().clone())
            .await?
            .content_key;
        let session = public.session();
        let docs = self.api().docs();
        let meta = docs
            .get_public_meta(token, link_id, &session.session_uid, &session.access_token)
            .await?;
        let commit_id = meta.commit_ids.last().cloned();
        let updates = if let Some(commit_id) = commit_id.as_deref() {
            let raw = docs
                .get_public_commit(
                    token,
                    link_id,
                    commit_id,
                    &session.session_uid,
                    &session.access_token,
                )
                .await?;
            decrypt_commit(&raw, &content_key)?
        } else {
            Vec::new()
        };
        Ok(OpenedDocument {
            uid,
            name,
            kind,
            meta,
            commit_id,
            updates,
        })
    }

    pub async fn open_document_uid(&self, uid: NodeUid) -> anyhow::Result<OpenedDocument> {
        let node = self
            .get_node(uid.clone())
            .await?
            .result()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let (name, media_type) = match &node {
            Node::File(f) | Node::Photo(f) => {
                (f.base.base.name.clone(), Some(f.base.media_type.as_str()))
            }
            Node::Folder(_) | Node::Album(_) => anyhow::bail!("node is a folder, not a document"),
        };
        let kind = DocumentKind::from_media_type(media_type).ok_or_else(|| {
            anyhow::anyhow!(
                "not a Proton Doc/Sheet (got {})",
                media_type.unwrap_or("unknown")
            )
        })?;

        let content_key = FileOperations::get_secrets(self, uid.clone())
            .await?
            .content_key;

        let docs = self.api().docs();
        let meta = match docs.get_meta(&uid.volume_id, &uid.link_id).await {
            Ok(meta) => meta,
            Err(_) => {
                docs.create_document(&uid.volume_id, &uid.link_id).await?;
                docs.get_meta(&uid.volume_id, &uid.link_id).await?
            }
        };

        let commit_id = meta.commit_ids.last().cloned();
        let updates = if let Some(commit_id) = commit_id.as_deref() {
            let raw = docs
                .get_commit(&uid.volume_id, &uid.link_id, commit_id)
                .await?;
            decrypt_commit(&raw, &content_key)?
        } else {
            Vec::new()
        };

        Ok(OpenedDocument {
            uid,
            name,
            kind,
            meta,
            commit_id,
            updates,
        })
    }

    pub async fn list_recent_documents(&self) -> anyhow::Result<Vec<RecentDocumentDto>> {
        self.api().docs().list_recent().await
    }

    pub async fn open_in_ddocs(
        &self,
        target: &str,
        api_url: &str,
        api_key: &str,
    ) -> anyhow::Result<(OpenedDocument, DdocPublish)> {
        self.open_in_ddocs_with_password(target, api_url, api_key, None)
            .await
    }

    pub async fn open_in_ddocs_with_password(
        &self,
        target: &str,
        api_url: &str,
        api_key: &str,
        password: Option<&str>,
    ) -> anyhow::Result<(OpenedDocument, DdocPublish)> {
        let doc = self.open_document_with_password(target, password).await?;
        let markdown = if doc.updates.is_empty() {
            String::new()
        } else {
            doc.to_markdown()?
        };
        anyhow::ensure!(
            !markdown.trim().is_empty() || !doc.name.is_empty(),
            "proton doc is empty"
        );
        let published = publish_to_ddocs(api_url, api_key, &doc.name, &markdown).await?;
        Ok((doc, published))
    }

    pub async fn write_document_markdown(
        &self,
        uid: NodeUid,
        markdown: &str,
    ) -> anyhow::Result<String> {
        let content_key = FileOperations::get_secrets(self, uid.clone())
            .await?
            .content_key;
        let author = self
            .account()
            .get_default_address()
            .await
            .map(|a| a.email_address)
            .unwrap_or_default();
        let yjs = markdown_to_yjs(markdown)?;
        let commit = encrypt_yjs_commit(&yjs, &content_key, &author)?;
        let docs = self.api().docs();
        let meta = docs.get_meta(&uid.volume_id, &uid.link_id).await.ok();
        if meta
            .as_ref()
            .map(|m| m.commit_ids.is_empty())
            .unwrap_or(true)
        {
            let _ = docs.create_document(&uid.volume_id, &uid.link_id).await;
            let seeded = docs
                .seed_initial_commit(&uid.volume_id, &uid.link_id, &commit.encode_to_vec())
                .await?;
            return Ok(seeded.commit_id);
        }
        let commit_id = meta.unwrap().commit_ids.last().cloned().unwrap();
        let lock_bytes = docs
            .lock_document(&uid.volume_id, &uid.link_id, Some(&commit_id))
            .await?;
        let lock = SquashLock::decode(lock_bytes.as_slice())?;
        let squash = SquashCommit {
            lock_id: lock.lock_id.clone(),
            commit_id: commit_id.clone(),
            commit: Some(commit),
        };
        let result = docs
            .squash_commit(
                &uid.volume_id,
                &uid.link_id,
                &commit_id,
                &squash.encode_to_vec(),
            )
            .await;
        let _ = docs
            .unlock_document(&uid.volume_id, &uid.link_id, &lock.lock_id)
            .await;
        result?;
        Ok(commit_id)
    }

    pub async fn sync_from_ddocs(
        &self,
        ddoc_id: &str,
        proton_target: &str,
        api_url: &str,
        api_key: &str,
        password: Option<&str>,
    ) -> anyhow::Result<String> {
        let (_title, markdown) = fetch_ddoc(api_url, api_key, ddoc_id).await?;
        let opened = self
            .open_document_with_password(proton_target, password)
            .await?;
        self.write_document_markdown(opened.uid, &markdown).await
    }
}

pub const PROTON_DOC_MIME: &str = PROTON_DOC_MEDIA_TYPE;
pub const PROTON_SHEET_MIME: &str = PROTON_SHEET_MEDIA_TYPE;

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    fn encrypt_docs_message(plaintext: &[u8], content_key: &PgpSessionKey, aad: &[u8]) -> Vec<u8> {
        let mut salt = vec![0u8; HKDF_SALT_SIZE];
        let mut iv = vec![0u8; GCM_IV_SIZE];
        rand::rng().fill_bytes(&mut salt);
        rand::rng().fill_bytes(&mut iv);
        let key = hkdf_sha256(&salt, &content_key.key, aad, 32).unwrap();
        let cipher = <Aes256Gcm16 as AesKeyInit>::new_from_slice(&key).unwrap();

        let nonce = GenericArray::<u8, U16>::from_slice(&iv);
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .unwrap();
        let mut out = salt;
        out.extend_from_slice(&iv);
        out.extend_from_slice(&ct);
        out
    }

    #[test]
    fn parses_docs_url_and_uid() {
        let target = parse_docs_target(
            "https://docs.proton.me/doc?type=doc&mode=open&volumeId=vol&linkId=link",
        )
        .unwrap();
        match target {
            DocsTarget::Private { uid } => {
                assert_eq!(uid.volume_id.raw(), "vol");
                assert_eq!(uid.link_id.raw(), "link");
            }
            _ => panic!("expected private"),
        }

        let public = parse_docs_target(
            "https://docs.proton.me/doc?type=doc&mode=open-url&token=tok&linkId=link",
        )
        .unwrap();
        match public {
            DocsTarget::Public {
                token,
                link_id,
                password,
            } => {
                assert_eq!(token, "tok");
                assert_eq!(link_id, "link");
                assert_eq!(password, None);
            }
            _ => panic!("expected public"),
        }

        let with_pw = parse_docs_target(
            "https://docs.proton.me/doc?mode=open-url&token=tok&linkId=link#secret",
        )
        .unwrap();
        match with_pw {
            DocsTarget::Public { password, .. } => assert_eq!(password.as_deref(), Some("secret")),
            _ => panic!("expected public with password"),
        }

        let uid = parse_docs_target("vol~link").unwrap();
        match uid {
            DocsTarget::Private { uid } => assert_eq!(uid.raw(), "vol~link"),
            _ => panic!("expected private uid"),
        }
    }

    #[test]
    fn decrypts_commit_roundtrip() {
        let key = PgpSessionKey {
            algorithm: 9,
            key: vec![7u8; 32],
        };
        let update = DocumentUpdate {
            encrypted_content: Vec::new(),
            version: 1,
            timestamp: 42,
            author_address: "a@proton.me".into(),
            uuid: "u1".into(),
        };
        let signed = SignedPlaintextContent {
            content: b"yjs-bytes".to_vec(),
            signature: b"sig".to_vec(),
        };
        let aad = associated_data(&update);
        let encrypted = encrypt_docs_message(&signed.encode_to_vec(), &key, aad.as_bytes());
        let commit = Commit {
            updates: Some(DocumentUpdateArray {
                document_updates: vec![DocumentUpdate {
                    encrypted_content: encrypted,
                    ..update
                }],
            }),
            version: 1,
            lock_id: String::new(),
        };
        let opened = decrypt_commit(&commit.encode_to_vec(), &key).unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].content, b"yjs-bytes");
        assert_eq!(opened[0].author_address, "a@proton.me");
    }

    #[test]
    fn merges_chunked_updates() {
        let header = |id, total, index, payload: &[u8]| {
            let mut out = CHUNK_SIGNAL.to_vec();
            out.extend_from_slice(&[0, id, total, index]);
            out.extend_from_slice(payload);
            out
        };
        let updates = vec![
            DocumentUpdate {
                encrypted_content: header(1, 2, 0, b"aa"),
                version: 1,
                timestamp: 1,
                author_address: "a".into(),
                uuid: "u".into(),
            },
            DocumentUpdate {
                encrypted_content: header(1, 2, 1, b"bb"),
                version: 1,
                timestamp: 1,
                author_address: "a".into(),
                uuid: "u".into(),
            },
        ];
        let merged = merge_chunked_updates(updates);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].encrypted_content, b"aabb");
    }

    #[test]
    fn xml_headings_become_markdown() {
        let md = xml_to_markdown(
            "<heading level=\"1\">Title</heading><paragraph>Hello <bold>world</bold></paragraph>",
        );
        assert!(md.contains("# Title"));
        assert!(md.contains("Hello **world**"));
    }

    #[test]
    fn yjs_text_update_to_markdown() {
        use yrs::{ReadTxn, Text, Transact};

        let src = yrs::Doc::new();
        {
            let text = src.get_or_insert_text("default");
            let mut txn = src.transact_mut();
            text.insert(&mut txn, 0, "hello ddocs");
        }
        let update = src
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        let md = updates_to_markdown(&[DecryptedUpdate {
            content: update,
            signature: Vec::new(),
            author_address: String::new(),
            timestamp: 0,
            uuid: String::new(),
        }])
        .unwrap();
        assert!(md.contains("hello ddocs"));
    }

    fn lexical_update() -> Vec<u8> {
        use yrs::{Any, MapPrelim, ReadTxn, Text, Transact, Xml, XmlTextPrelim};

        let src = yrs::Doc::new();
        let root = src.get_or_insert_text("root");
        let mut txn = src.transact_mut();
        let text_node = |format: i64| {
            MapPrelim::from([
                ("__type".to_string(), Any::from("text")),
                ("__format".to_string(), Any::from(format)),
            ])
        };
        let heading = root.insert_embed(&mut txn, 0, XmlTextPrelim::new(""));
        heading.insert_attribute(&mut txn, "__type", "heading");
        heading.insert_attribute(&mut txn, "__tag", "h2");
        heading.insert_embed(&mut txn, 0, text_node(0));
        heading.insert(&mut txn, 1, "Plan");
        let paragraph = root.insert_embed(&mut txn, 1, XmlTextPrelim::new(""));
        paragraph.insert_attribute(&mut txn, "__type", "paragraph");
        paragraph.insert_embed(&mut txn, 0, text_node(0));
        paragraph.insert(&mut txn, 1, "Ship ");
        paragraph.insert_embed(&mut txn, 6, text_node(1));
        paragraph.insert(&mut txn, 7, "today");
        let list = root.insert_embed(&mut txn, 2, XmlTextPrelim::new(""));
        list.insert_attribute(&mut txn, "__type", "list");
        list.insert_attribute(&mut txn, "__listType", "number");
        for (index, item) in ["one", "two"].into_iter().enumerate() {
            let entry = list.insert_embed(&mut txn, index as u32, XmlTextPrelim::new(""));
            entry.insert_attribute(&mut txn, "__type", "listitem");
            entry.insert_embed(&mut txn, 0, text_node(0));
            entry.insert(&mut txn, 1, item);
        }
        drop(txn);
        src.transact()
            .encode_state_as_update_v1(&yrs::StateVector::default())
    }

    fn decrypted(content: Vec<u8>) -> DecryptedUpdate {
        DecryptedUpdate {
            content,
            signature: Vec::new(),
            author_address: String::new(),
            timestamp: 0,
            uuid: String::new(),
        }
    }

    #[test]
    fn lexical_documents_export_as_markdown() {
        let update = lexical_update();
        // One editing client, so the bare update starts like a sync message.
        assert_eq!(update[0], 1);
        let md = updates_to_markdown(&[decrypted(update)]).unwrap();
        assert_eq!(md, "## Plan\n\nShip **today**\n\n1. one\n2. two");
    }

    #[test]
    fn gzipped_updates_are_inflated() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&lexical_update()).unwrap();
        let md = updates_to_markdown(&[decrypted(gz.finish().unwrap())]).unwrap();
        assert!(md.starts_with("## Plan"));
    }

    #[test]
    fn ddocs_link_falls_back() {
        let published = DdocPublish {
            ddoc_id: "abc".into(),
            title: None,
            sync_status: Some("pending".into()),
            link: None,
        };
        assert_eq!(published.open_url(), "https://ddocs.new/d/abc");
        let shared = DdocPublish {
            ddoc_id: "abc".into(),
            title: None,
            sync_status: Some("synced".into()),
            link: Some("https://ddocs.new/d/abc#guestkey".into()),
        };
        assert_eq!(shared.owner_url(), "https://ddocs.new/d/abc");
        assert_eq!(shared.share_url(), Some("https://ddocs.new/d/abc#guestkey"));
    }

    #[test]
    fn strips_title_heading_from_body() {
        assert_eq!(strip_duplicate_title("# Notes\n\nhello", "Notes"), "hello");
        assert_eq!(strip_duplicate_title("hello", "Notes"), "hello");
    }

    #[test]
    fn markdown_roundtrips_through_yjs() {
        let yjs = markdown_to_yjs("# Hello\n\nworld").unwrap();
        let md = updates_to_markdown(&[DecryptedUpdate {
            content: yjs,
            signature: Vec::new(),
            author_address: String::new(),
            timestamp: 0,
            uuid: String::new(),
        }])
        .unwrap();
        assert!(md.contains("Hello") || md.contains("world"));
    }

    #[test]
    fn live_tick_writes_after_settle() {
        let mut seen = "a".into();
        let written = "a".to_string();
        let mut stable = 0;
        assert_eq!(
            live_sync_tick("b", &mut seen, &written, &mut stable),
            LiveSyncTick::Changed
        );
        assert_eq!(
            live_sync_tick("b", &mut seen, &written, &mut stable),
            LiveSyncTick::Idle
        );
        assert_eq!(
            live_sync_tick("b", &mut seen, &written, &mut stable),
            LiveSyncTick::Write
        );
        assert_eq!(
            live_sync_tick("a", &mut seen, &written, &mut stable),
            LiveSyncTick::Changed
        );
        assert_eq!(
            live_sync_tick("a", &mut seen, &written, &mut stable),
            LiveSyncTick::Idle
        );
    }

    #[test]
    fn reads_y_protocols_wrapped_update() {
        use yrs::{ReadTxn, Text, Transact};
        let src = yrs::Doc::new();
        {
            let text = src.get_or_insert_text("weird-root");
            let mut txn = src.transact_mut();
            text.insert(&mut txn, 0, "secret notes");
        }
        let inner = src
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        let mut wrapped = vec![0, 2];
        wrapped.extend_from_slice(&inner);
        let md = updates_to_markdown(&[DecryptedUpdate {
            content: wrapped,
            signature: Vec::new(),
            author_address: String::new(),
            timestamp: 0,
            uuid: String::new(),
        }])
        .unwrap();
        assert!(md.contains("secret notes"));
    }

    #[test]
    fn reads_lexical_json_update() {
        let json = br#"{"root":{"children":[{"text":"hello from lexical"}]}}"#;
        let md = updates_to_markdown(&[DecryptedUpdate {
            content: json.to_vec(),
            signature: Vec::new(),
            author_address: String::new(),
            timestamp: 0,
            uuid: String::new(),
        }])
        .unwrap();
        assert!(md.contains("hello from lexical"));
    }
}
