//! Readable-content extraction from arbitrary HTML pages.
//!
//! A page is reduced once to a list of [`Block`]s (headings, paragraphs, list
//! items, code, quotes, table rows). That is what gets cached; each request then
//! renders it as plain text or Markdown within its own character budget.

use std::{borrow::Cow, collections::HashMap, sync::LazyLock};

use ego_tree::{NodeId, iter::Edge};
use scraper::{Html, Node, Selector};
use url::Url;

use super::STOPWORDS;
use crate::{engines::util::sel, model::ContentFormat};

static ROOTS: LazyLock<[Selector; 4]> =
    LazyLock::new(|| [sel("article"), sel("main"), sel("[role='main']"), sel("body")]);

/// Subtrees that never contain primary content.
const SKIP: &[&str] = &[
    "script", "style", "noscript", "svg", "nav", "footer", "header", "aside", "form", "iframe", "template", "button",
    "select", "canvas", "video", "audio", "picture", "head",
];

/// Generic block containers: each starts a new paragraph (outside lists/tables).
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "dd",
    "dt",
    "dl",
    "figure",
    "figcaption",
    "details",
    "summary",
    "address",
    "center",
    "hr",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    /// `None` for a bullet, `Some(n)` for the n-th item of an ordered list.
    ListItem(Option<u32>),
    Code,
    Quote,
    /// A table row; the first row of each table is rendered as its header.
    TableRow {
        header: bool,
        cells: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Plain text.
    pub text: String,
    /// The same text with inline Markdown (links, emphasis, inline code).
    pub md: String,
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub blocks: Vec<Block>,
    /// Words in the whole readable page, including any part past the extraction limit.
    pub word_count: usize,
    /// Extraction stopped at its character limit.
    pub truncated: bool,
}

impl Page {
    /// Approximate heap size, for cache weighing.
    pub fn weight(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| b.text.len() + b.md.len() + 32)
            .sum::<usize>()
            + 64
    }

    /// Render at most `max_chars` characters. Returns the content and whether
    /// anything of the page was left out.
    pub fn render(&self, format: ContentFormat, max_chars: usize) -> (String, bool) {
        self.render_blocks(self.blocks.iter(), format, max_chars)
    }

    /// Render only the blocks that best match `query` (BM25), each with the
    /// heading it sits under, in page order, gaps marked with `…`. Falls back to
    /// [`Page::render`] when nothing on the page matches.
    pub fn render_relevant(&self, query: &str, format: ContentFormat, max_chars: usize) -> (String, bool) {
        let terms = query_terms(query);
        let docs: Vec<Vec<String>> = self.blocks.iter().map(|b| tokens(&b.text)).collect();
        let scores = bm25(&terms, &docs);
        let mut ranked: Vec<usize> = (0..self.blocks.len()).filter(|&i| scores[i] > 0.0).collect();
        if ranked.is_empty() {
            return self.render(format, max_chars);
        }
        ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));

        // Greedily take the best blocks (and their headings) that fit the budget.
        let cost = |i: usize| self.blocks[i].md.chars().count() + 4;
        let mut picked = vec![false; self.blocks.len()];
        let mut used = 0;
        for i in ranked {
            let heading = (0..i)
                .rev()
                .find(|&j| matches!(self.blocks[j].kind, BlockKind::Heading(_)))
                .filter(|&j| !picked[j]);
            let need = cost(i) + heading.map_or(0, cost);
            if used + need > max_chars {
                continue;
            }
            picked[i] = true;
            if let Some(h) = heading {
                picked[h] = true;
            }
            used += need;
        }
        if !picked.iter().any(|p| *p) {
            // Even the best passage is over budget: cut it rather than return nothing.
            let best = scores
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map_or(0, |(i, _)| i);
            return self.render_blocks(std::iter::once(&self.blocks[best]), format, max_chars);
        }

        let gap = Block {
            kind: BlockKind::Paragraph,
            text: "…".into(),
            md: "…".into(),
        };
        let mut chosen = Vec::new();
        let mut last: Option<usize> = None;
        for i in (0..self.blocks.len()).filter(|&i| picked[i]) {
            if last.is_some_and(|l| i > l + 1) || (last.is_none() && i > 0) {
                chosen.push(&gap);
            }
            chosen.push(&self.blocks[i]);
            last = Some(i);
        }
        if last.is_some_and(|l| l + 1 < self.blocks.len()) {
            chosen.push(&gap);
        }
        let (out, _) = self.render_blocks(chosen.into_iter(), format, max_chars);
        let all = picked.iter().all(|p| *p);
        (out, self.truncated || !all)
    }

    pub(crate) fn render_blocks<'a>(
        &self,
        blocks: impl Iterator<Item = &'a Block>,
        format: ContentFormat,
        max_chars: usize,
    ) -> (String, bool) {
        let mut out = String::new();
        let mut used = 0;
        let mut prev: Option<BlockKind> = None;
        for b in blocks {
            let sep = separator(format, prev, b.kind);
            let body = render_block(format, b);
            let need = sep.len() + body.chars().count();
            if used + need > max_chars {
                // Fit what's left of this block as plain text, cut on a word
                // (code keeps its fences so the Markdown stays well-formed).
                let fenced = format == ContentFormat::Markdown && b.kind == BlockKind::Code;
                let (open, close) = if fenced { ("```\n", "\n```") } else { ("", "") };
                let prefix = prefix(format, b.kind);
                let overhead = sep.len() + prefix.chars().count() + open.len() + close.len();
                let room = max_chars.saturating_sub(used + overhead);
                let cut = truncate_text(&b.text, room);
                if room >= 10 && !cut.is_empty() {
                    out.push_str(sep);
                    out.push_str(&prefix);
                    out.push_str(open);
                    out.push_str(cut);
                    out.push_str(close);
                }
                return (out, true);
            }
            out.push_str(sep);
            out.push_str(&body);
            used += need;
            prev = Some(b.kind);
        }
        (out, self.truncated)
    }
}

fn separator(format: ContentFormat, prev: Option<BlockKind>, next: BlockKind) -> &'static str {
    match (format, prev, next) {
        (_, None, _) => "",
        (ContentFormat::Text, ..) => "\n",
        // Items of the same list stay together; a different list needs a blank line.
        (_, Some(BlockKind::ListItem(None)), BlockKind::ListItem(None)) => "\n",
        (_, Some(BlockKind::ListItem(Some(a))), BlockKind::ListItem(Some(b))) if b == a + 1 => "\n",
        (_, Some(BlockKind::TableRow { .. }), BlockKind::TableRow { header: false, .. }) => "\n",
        _ => "\n\n",
    }
}

fn prefix(format: ContentFormat, kind: BlockKind) -> Cow<'static, str> {
    if format == ContentFormat::Text {
        return "".into();
    }
    match kind {
        BlockKind::Heading(n) => format!("{} ", "#".repeat(n.into())).into(),
        BlockKind::ListItem(None) => "- ".into(),
        BlockKind::ListItem(Some(n)) => format!("{n}. ").into(),
        BlockKind::Quote => "> ".into(),
        BlockKind::Paragraph | BlockKind::Code | BlockKind::TableRow { .. } => "".into(),
    }
}

fn render_block(format: ContentFormat, b: &Block) -> Cow<'_, str> {
    if format == ContentFormat::Text {
        return Cow::Borrowed(&b.text);
    }
    match b.kind {
        BlockKind::Code => format!("```\n{}\n```", b.text).into(),
        BlockKind::TableRow { header, cells } => {
            let mut row = format!("| {} |", b.md);
            if header {
                row.push_str("\n|");
                for _ in 0..cells.max(1) {
                    row.push_str(" --- |");
                }
            }
            row.into()
        }
        kind => match prefix(format, kind) {
            Cow::Borrowed("") => Cow::Borrowed(&b.md),
            p => format!("{p}{}", b.md).into(),
        },
    }
}

/// Lowercased alphanumeric words.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Distinct, meaningful query words (operators such as `site:x` excluded).
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split_whitespace()
        .filter(|w| !w.contains(':') && !w.starts_with('-'))
        .flat_map(tokens)
        .filter(|t| t.chars().count() >= 2 && !STOPWORDS.contains(&t.as_str()))
        .collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Okapi BM25 score of every doc for `terms` (k1 = 1.2, b = 0.75).
fn bm25(terms: &[String], docs: &[Vec<String>]) -> Vec<f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let n = docs.len() as f64;
    let avg = (docs.iter().map(Vec::len).sum::<usize>() as f64 / n.max(1.0)).max(1.0);
    let idf: HashMap<&str, f64> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.iter().any(|w| w == t)).count() as f64;
            (t.as_str(), (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .collect();
    docs.iter()
        .map(|d| {
            let len = d.len() as f64;
            terms
                .iter()
                .map(|t| {
                    let tf = d.iter().filter(|w| *w == t).count() as f64;
                    if tf == 0.0 {
                        return 0.0;
                    }
                    idf[t.as_str()] * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg))
                })
                .sum()
        })
        .collect()
}

/// At most `max` characters of `text`, cut at a word boundary when possible.
pub fn truncate_text(text: &str, max: usize) -> &str {
    let Some((cut, _)) = text.char_indices().nth(max) else {
        return text;
    };
    let head = &text[..cut];
    match head.rfind(char::is_whitespace) {
        Some(ws) if ws > cut / 2 => head[..ws].trim_end(),
        _ => head,
    }
}

enum InlineKind {
    Link(String),
    Marker(&'static str),
}

/// An open inline element. `start` is its offset in the Markdown buffer
/// (`usize::MAX` once a block boundary inside it invalidated the offset).
struct Inline {
    node: NodeId,
    kind: InlineKind,
    start: usize,
}

struct Builder<'a> {
    base: Option<&'a Url>,
    max_chars: usize,
    chars: usize,
    page: Page,
    kind: BlockKind,
    text: String,
    md: String,
    /// Whitespace was seen since the last word: owe a space before the next one.
    space: bool,
    /// Open lists: `None` = bullets, `Some(count)` = ordered.
    lists: Vec<Option<u32>>,
    li: usize,
    quote: usize,
    pre: usize,
    /// Current table row: (cells so far, rows so far in this table).
    row: Option<u16>,
    table_rows: u32,
    inline: Vec<Inline>,
    full: bool,
}

impl<'a> Builder<'a> {
    fn new(base: Option<&'a Url>, max_chars: usize) -> Self {
        Self {
            base,
            max_chars,
            chars: 0,
            page: Page::default(),
            kind: BlockKind::Paragraph,
            text: String::new(),
            md: String::new(),
            space: false,
            lists: Vec::new(),
            li: 0,
            quote: 0,
            pre: 0,
            row: None,
            table_rows: 0,
            inline: Vec::new(),
            full: false,
        }
    }

    /// Kind of the next block given where we are in the tree.
    fn context_kind(&self) -> BlockKind {
        if self.pre > 0 {
            BlockKind::Code
        } else if self.quote > 0 {
            BlockKind::Quote
        } else {
            BlockKind::Paragraph
        }
    }

    /// Inside a list item or table cell, block containers are just whitespace.
    fn inline_context(&self) -> bool {
        self.li > 0 || self.row.is_some()
    }

    fn emit_space(&mut self) {
        if self.space && !self.text.is_empty() {
            self.text.push(' ');
            self.md.push(' ');
        }
        self.space = false;
    }

    fn push_text(&mut self, raw: &str) {
        let words = raw.split_whitespace().count();
        self.page.word_count += words;
        if self.full {
            self.page.truncated |= words > 0;
            return;
        }
        if self.pre > 0 {
            self.text.push_str(raw);
            self.md.push_str(raw);
            return;
        }
        if raw.starts_with(char::is_whitespace) {
            self.space = true;
        }
        for (i, word) in raw.split_whitespace().enumerate() {
            if i > 0 {
                self.space = true;
            }
            self.emit_space();
            self.text.push_str(word);
            self.md.push_str(word);
        }
        if raw.ends_with(char::is_whitespace) {
            self.space = true;
        }
    }

    fn flush(&mut self) {
        let text = if self.kind == BlockKind::Code {
            self.text.trim_matches(['\n', '\r']).trim_end()
        } else {
            self.text.trim()
        };
        if !text.is_empty() && !self.full {
            let n = text.chars().count();
            let room = self.max_chars.saturating_sub(self.chars);
            let md = if self.kind == BlockKind::Code {
                text
            } else {
                self.md.trim()
            };
            if n <= room {
                self.page.blocks.push(Block {
                    kind: self.kind,
                    text: text.to_owned(),
                    md: md.to_owned(),
                });
                self.chars += n;
            } else {
                // Inline Markdown can't be cut safely; keep the plain words.
                let cut = truncate_text(text, room);
                if !cut.is_empty() {
                    self.page.blocks.push(Block {
                        kind: self.kind,
                        text: cut.to_owned(),
                        md: cut.to_owned(),
                    });
                }
                self.full = true;
                self.page.truncated = true;
            }
        }
        self.text.clear();
        self.md.clear();
        self.space = false;
        for i in &mut self.inline {
            i.start = usize::MAX;
        }
    }

    fn open<'n>(&mut self, id: NodeId, name: &str, attr: impl Fn(&str) -> Option<&'n str>) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !self.inline_context() => {
                self.flush();
                self.kind = BlockKind::Heading(name.as_bytes()[1] - b'0');
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.push((name == "ol").then_some(0));
            }
            "li" => {
                self.flush();
                self.li += 1;
                let n = match self.lists.last_mut() {
                    Some(Some(count)) => {
                        *count += 1;
                        Some(*count)
                    }
                    _ => None,
                };
                self.kind = BlockKind::ListItem(n);
            }
            "pre" => {
                self.flush();
                self.pre += 1;
                self.kind = BlockKind::Code;
            }
            "blockquote" => {
                self.flush();
                self.quote += 1;
                self.kind = BlockKind::Quote;
            }
            "table" => {
                self.flush();
                self.table_rows = 0;
            }
            "tr" => {
                self.flush();
                self.row = Some(0);
            }
            "td" | "th" => {
                if let Some(cells) = &mut self.row {
                    if *cells > 0 {
                        self.text.push_str(" | ");
                        self.md.push_str(" | ");
                    }
                    *cells += 1;
                    self.space = false;
                }
            }
            "br" => {
                if self.pre > 0 {
                    self.text.push('\n');
                    self.md.push('\n');
                } else if self.inline_context() {
                    self.space = true;
                } else {
                    self.flush();
                }
            }
            "a" if self.pre == 0 => {
                let href = attr("href").and_then(|h| self.resolve(h));
                if let Some(url) = href {
                    self.emit_space();
                    self.inline.push(Inline {
                        node: id,
                        kind: InlineKind::Link(url),
                        start: self.md.len(),
                    });
                }
            }
            "strong" | "b" | "em" | "i" | "code" if self.pre == 0 => {
                let marker = match name {
                    "strong" | "b" => "**",
                    "code" => "`",
                    _ => "_",
                };
                self.emit_space();
                self.md.push_str(marker);
                self.inline.push(Inline {
                    node: id,
                    kind: InlineKind::Marker(marker),
                    start: self.md.len(),
                });
            }
            n if BLOCK.contains(&n) => {
                if self.inline_context() {
                    self.space = true;
                } else {
                    self.flush();
                    self.kind = self.context_kind();
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, id: NodeId, name: &str) {
        if self.inline.last().is_some_and(|i| i.node == id) {
            let Inline { kind, start, .. } = self.inline.pop().expect("checked");
            if start > self.md.len() {
                return;
            }
            match kind {
                InlineKind::Marker(m) => {
                    if self.md[start..].trim().is_empty() {
                        // Empty emphasis (icon fonts etc.): drop the opening marker.
                        self.md.truncate(start - m.len());
                    } else {
                        self.md.push_str(m);
                    }
                }
                InlineKind::Link(url) => {
                    let label = self.md[start..].trim().to_owned();
                    if !label.is_empty() {
                        self.md.truncate(start);
                        self.md.push('[');
                        self.md.push_str(&label);
                        self.md.push_str("](");
                        self.md.push_str(&url);
                        self.md.push(')');
                    }
                }
            }
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !self.inline_context() => {
                self.flush();
                self.kind = self.context_kind();
            }
            "ul" | "ol" => {
                self.flush();
                self.lists.pop();
                self.kind = self.context_kind();
            }
            "li" => {
                self.flush();
                self.li = self.li.saturating_sub(1);
                self.kind = self.context_kind();
            }
            "pre" => {
                self.flush();
                self.pre = self.pre.saturating_sub(1);
                self.kind = self.context_kind();
            }
            "blockquote" => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.kind = self.context_kind();
            }
            "tr" => {
                if let Some(cells) = self.row.take() {
                    self.kind = BlockKind::TableRow {
                        header: self.table_rows == 0,
                        cells,
                    };
                    let before = self.page.blocks.len();
                    self.flush();
                    self.table_rows += u32::from(self.page.blocks.len() > before);
                }
                self.kind = self.context_kind();
            }
            "table" => {
                self.flush();
                self.row = None;
                self.kind = self.context_kind();
            }
            n if BLOCK.contains(&n) && !self.inline_context() => {
                self.flush();
                self.kind = self.context_kind();
            }
            _ => {}
        }
    }

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let url = match self.base {
            Some(base) => base.join(href).ok()?,
            None => Url::parse(href).ok()?,
        };
        matches!(url.scheme(), "http" | "https").then(|| url.into())
    }
}

/// Extract the main readable content of `html`, keeping at most `max_chars`
/// characters of text. Relative links resolve against `base`.
pub fn extract(html: &str, base: Option<&Url>, max_chars: usize) -> Page {
    let doc = Html::parse_document(html);
    let root = ROOTS
        .iter()
        .find_map(|s| doc.select(s).next())
        .unwrap_or_else(|| doc.root_element());

    let mut b = Builder::new(base, max_chars);
    let mut skip_depth = 0usize;
    for edge in root.traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(el) if skip_depth > 0 || SKIP.contains(&el.name()) => skip_depth += 1,
                Node::Element(el) => b.open(node.id(), el.name(), |a| el.attr(a)),
                Node::Text(t) if skip_depth == 0 => b.push_text(t),
                _ => {}
            },
            Edge::Close(node) => {
                if let Node::Element(el) = node.value() {
                    if skip_depth > 0 {
                        skip_depth -= 1;
                    } else {
                        b.close(node.id(), el.name());
                    }
                }
            }
        }
    }
    b.flush();
    b.page
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(html: &str, max: usize) -> String {
        extract(html, None, max).render(ContentFormat::Text, max).0
    }

    fn md(html: &str) -> String {
        let base = Url::parse("https://example.com/docs/").unwrap();
        extract(html, Some(&base), 10_000)
            .render(ContentFormat::Markdown, 10_000)
            .0
    }

    #[test]
    fn extracts_main_content_and_skips_chrome() {
        let html = r#"<html><head><title>t</title><style>.x{}</style></head><body>
            <nav>Home About</nav>
            <article><h1>Tokio  guide</h1><p>Async <b>runtime</b> for Rust.</p>
            <script>var x = 1;</script><p>Second paragraph.</p></article>
            <footer>Copyright</footer></body></html>"#;
        assert_eq!(
            text(html, 1000),
            "Tokio guide\nAsync runtime for Rust.\nSecond paragraph."
        );
    }

    #[test]
    fn renders_structured_markdown() {
        let html = r##"<body><main>
            <h2>Install</h2>
            <p>Run <code>cargo add tokio</code>, see <a href="../guide">the <em>guide</em></a>.</p>
            <ul><li>fast</li><li><p>reliable</p></li></ul>
            <ol><li>one</li><li>two</li></ol>
            <pre><code>fn main() {
    run();
}</code></pre>
            <blockquote><p>Quoted</p></blockquote>
            <table><tr><th>Name</th><th>Kind</th></tr><tr><td>tokio</td><td>runtime</td></tr></table>
            <p><i class="icon"></i>Plain <a href="#top">anchor</a> and <a href="mailto:x@y.z">mail</a></p>
        </main></body>"##;
        assert_eq!(
            md(html),
            "## Install\n\n\
             Run `cargo add tokio`, see [the _guide_](https://example.com/guide).\n\n\
             - fast\n- reliable\n\n\
             1. one\n2. two\n\n\
             ```\nfn main() {\n    run();\n}\n```\n\n\
             > Quoted\n\n\
             | Name | Kind |\n| --- | --- |\n| tokio | runtime |\n\n\
             Plain anchor and mail"
        );
    }

    #[test]
    fn spaces_follow_the_source_not_the_markup() {
        assert_eq!(text("<p>links and <b>markup</b>.</p>", 100), "links and markup.");
        assert_eq!(text("<p>a<b>b</b> c</p>", 100), "ab c");
        assert_eq!(md("<p>see <b>bold </b>text</p>"), "see **bold** text");
    }

    #[test]
    fn respects_max_chars() {
        let html = "<body><p>one two three four five</p></body>";
        let out = text(html, 9);
        assert_eq!(out, "one two");
        let page = extract(html, None, 9);
        assert!(page.truncated);
        assert_eq!(page.word_count, 5, "word count covers the whole page");
        let (_, truncated) = extract(html, None, 1000).render(ContentFormat::Text, 1000);
        assert!(!truncated);
    }

    #[test]
    fn render_budget_cuts_on_words() {
        let page = extract(
            "<p>first paragraph here</p><p>second paragraph that is long</p>",
            None,
            1000,
        );
        let (out, truncated) = page.render(ContentFormat::Markdown, 40);
        assert_eq!(out, "first paragraph here\n\nsecond paragraph");
        assert!(truncated && out.chars().count() <= 40);
    }

    #[test]
    fn truncates_on_word_boundaries() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(truncate_text("hello brave new world", 13), "hello brave");
        assert_eq!(truncate_text("héllo wörld", 7), "héllo");
        assert_eq!(truncate_text("abcdefghij", 4), "abcd");
    }

    #[test]
    fn cut_code_blocks_keep_their_fences() {
        let page = extract("<p>intro</p><pre>line one\nline two\nline three</pre>", None, 1000);
        let (out, truncated) = page.render(ContentFormat::Markdown, 30);
        assert!(truncated);
        assert!(
            out.starts_with("intro\n\n```\nline") && out.ends_with("\n```"),
            "{out:?}"
        );
        assert!(out.chars().count() <= 30);
    }

    #[test]
    fn relevant_mode_keeps_matching_passages_with_headings() {
        let mut html = String::from("<body><h1>Cooking</h1><p>Bread needs flour and water.</p>");
        for i in 0..20 {
            html.push_str(&format!(
                "<p>Unrelated filler paragraph number {i} about gardening.</p>"
            ));
        }
        html.push_str("<h2>Async runtimes</h2><p>Tokio schedules tasks on a work-stealing runtime.</p>");
        html.push_str("<p>More gardening filler.</p></body>");
        let page = extract(&html, None, 100_000);
        let (out, truncated) = page.render_relevant("tokio runtime site:docs.rs", ContentFormat::Markdown, 300);
        assert_eq!(
            out,
            "…\n\n## Async runtimes\n\nTokio schedules tasks on a work-stealing runtime.\n\n…"
        );
        assert!(truncated);
        // Nothing matches: fall back to the lead of the page.
        let (lead, _) = page.render_relevant("quantum chromodynamics", ContentFormat::Text, 40);
        assert!(lead.starts_with("Cooking\nBread"), "{lead}");
    }

    #[test]
    fn bm25_prefers_rarer_terms_and_denser_matches() {
        let docs = vec![tokens("rust rust rust"), tokens("rust tokio"), tokens("gardening")];
        let s = bm25(&query_terms("rust tokio"), &docs);
        assert!(s[1] > s[0], "matching both terms beats repeating one: {s:?}");
        assert_eq!(s[2], 0.0);
    }

    #[test]
    fn handles_unicode_limits() {
        let html = "<body><p>héllo wörld ünïcode</p></body>";
        assert_eq!(text(html, 11), "héllo wörld");
    }

    #[test]
    fn survives_inline_elements_spanning_blocks() {
        // A link wrapping block content must not corrupt the buffers.
        let out = md(r#"<a href="/x"><div>Card title</div><p>Card body</p></a><p>after</p>"#);
        assert!(
            out.contains("Card title") && out.contains("Card body") && out.ends_with("after"),
            "{out}"
        );
    }
}
