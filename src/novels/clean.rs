//! A chapter's HTML made into clean XHTML for the book: text and its
//! formatting, tables, images; no scripts, styles, ads, forms or hidden
//! elements (Royal Road hides watermark paragraphs with a page style).
//! EPUB readers want well-formed XHTML, so it's written out tag by tag
//! rather than passed through.

use scraper::{ElementRef, Html, Node};

/// Tags kept as they are.
const KEPT: &[&str] = &[
    "p", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6", "em", "i", "strong", "b", "u", "s", "del", "sub", "sup",
    "small", "blockquote", "ul", "ol", "li", "table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "pre",
    "code", "figure", "figcaption", "a", "img", "div",
];
/// Tags dropped with everything in them.
const DROPPED: &[&str] = &[
    "script", "style", "iframe", "noscript", "form", "button", "input", "select", "textarea", "svg", "video", "audio",
    "object", "embed", "ins", "nav", "template", "canvas", "link", "meta",
];
const VOID: &[&str] = &["br", "hr", "img"];

/// `html` (a fragment) as XHTML. `hidden` are class names the page hides;
/// `base` resolves relative links; without `images`, images become their
/// alt text.
pub fn clean(html: &str, hidden: &[String], base: &str, images: bool) -> String {
    let fragment = Html::parse_fragment(html);
    let base = url::Url::parse(base).ok();
    let mut out = String::with_capacity(html.len());
    let context = Context { hidden, base: base.as_ref(), images };
    for child in fragment.root_element().children() {
        write_node(child, &context, &mut out);
    }
    out
}

struct Context<'a> {
    hidden: &'a [String],
    base: Option<&'a url::Url>,
    images: bool,
}

fn write_node(node: ego_tree::NodeRef<'_, Node>, context: &Context, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&escape(text)),
        Node::Element(_) => {
            if let Some(element) = ElementRef::wrap(node) {
                write_element(element, context, out);
            }
        }
        _ => {}
    }
}

fn write_element(element: ElementRef<'_>, context: &Context, out: &mut String) {
    let value = element.value();
    let name = value.name().to_ascii_lowercase();
    if DROPPED.contains(&name.as_str()) || is_hidden(element, context.hidden) {
        return;
    }
    if name == "img" {
        write_image(element, context, out);
        return;
    }
    // Unknown tags (span, center, font, section…) keep their content only;
    // `center` keeps its centring.
    let tag = match name.as_str() {
        "center" => Some("div"),
        n if KEPT.contains(&n) => Some(n),
        _ => None,
    };
    let Some(tag) = tag else {
        for child in element.children() {
            write_node(child, context, out);
        }
        return;
    };

    out.push('<');
    out.push_str(tag);
    if name == "center" {
        out.push_str(" style=\"text-align: center\"");
    } else if let Some(align) = alignment(element) {
        out.push_str(&format!(" style=\"text-align: {align}\""));
    }
    if tag == "a" {
        if let Some(href) = element.attr("href").and_then(|h| absolute(h, context.base)) {
            if href.starts_with("http://") || href.starts_with("https://") {
                out.push_str(&format!(" href=\"{}\"", escape(&href)));
            }
        }
    }
    if tag == "td" || tag == "th" {
        for attribute in ["colspan", "rowspan"] {
            if let Some(n) = element.attr(attribute).filter(|n| n.chars().all(|c| c.is_ascii_digit())) {
                out.push_str(&format!(" {attribute}=\"{n}\""));
            }
        }
    }
    if VOID.contains(&tag) {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for child in element.children() {
        write_node(child, context, out);
    }
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

/// An image: its real source (lazy-loading pages keep it in data-src), its
/// srcset for the "Small images" option, and its alt text.
fn write_image(element: ElementRef<'_>, context: &Context, out: &mut String) {
    let alt = element.attr("alt").unwrap_or("").trim().to_string();
    if !context.images {
        if !alt.is_empty() {
            out.push_str(&format!("<span class=\"image-alt\">[{}]</span>", escape(&alt)));
        }
        return;
    }
    let placeholder = |s: &&str| s.is_empty() || s.starts_with("data:");
    let src = ["data-src", "data-lazy-src", "data-original", "src"]
        .iter()
        .filter_map(|a| element.attr(a))
        .find(|s| !placeholder(s))
        .and_then(|s| absolute(s, context.base));
    let Some(src) = src else {
        return; // nothing to show (Royal Road has these)
    };
    out.push_str(&format!("<img src=\"{}\" alt=\"{}\"", escape(&src), escape(&alt)));
    let srcset = element.attr("data-srcset").or_else(|| element.attr("srcset"));
    if let Some(srcset) = srcset {
        // Resolved here, read (and removed) by images::Collector.
        let resolved: Vec<String> = srcset
            .split(',')
            .filter_map(|candidate| {
                let mut parts = candidate.split_whitespace();
                let url = absolute(parts.next()?, context.base)?;
                Some(format!("{url} {}", parts.next().unwrap_or("1x")))
            })
            .collect();
        out.push_str(&format!(" data-eddy-srcset=\"{}\"", escape(&resolved.join(", "))));
    }
    out.push_str("/>");
}

/// Hidden by one of the page's styles, or by its own.
fn is_hidden(element: ElementRef<'_>, hidden: &[String]) -> bool {
    if element.value().classes().any(|c| hidden.iter().any(|h| h == c)) {
        return true;
    }
    element.attr("style").is_some_and(|s| {
        let s = s.to_ascii_lowercase().replace(' ', "");
        s.contains("display:none") || s.contains("visibility:hidden")
    })
}

/// A paragraph's text-align, the one bit of styling worth keeping.
fn alignment(element: ElementRef<'_>) -> Option<&'static str> {
    let style = element.attr("style")?.to_ascii_lowercase().replace(' ', "");
    ["center", "right", "justify"].into_iter().find(|a| style.contains(&format!("text-align:{a}")))
}

fn absolute(link: &str, base: Option<&url::Url>) -> Option<String> {
    let link = link.trim();
    match base {
        Some(base) => base.join(link).ok().map(|u| u.to_string()),
        None => url::Url::parse(link).ok().map(|u| u.to_string()),
    }
}

/// Text and attribute values for XML.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Not allowed in XML at all.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// Class names a page's `<style>` blocks hide (`.abc { display: none }`).
pub fn hidden_classes(page: &Html) -> Vec<String> {
    let style = scraper::Selector::parse("style").unwrap();
    let rule = regex::Regex::new(r"\.([A-Za-z0-9_-]+)\s*\{[^}]*display\s*:\s*none").unwrap();
    page.select(&style)
        .flat_map(|s| rule.captures_iter(&s.inner_html()).map(|c| c[1].to_string()).collect::<Vec<_>>())
        .collect()
}

/// A name for a folder or file inside the cache.
pub fn safe_component(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// A book's file name that's safe on every filesystem.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() { "book".to_string() } else { trimmed.chars().take(150).collect() }
}
