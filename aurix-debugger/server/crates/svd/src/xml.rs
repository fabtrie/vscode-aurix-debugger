//! The SVD file as a stream of the elements the conversion needs: the device's defaults and, one
//! at a time, its peripherals as small element trees.
//!
//! An element tree has the shape the TypeScript conversion worked on (xml2js with default
//! options): text is not trimmed, entities are decoded as there (an unknown one stays as written),
//! and whitespace between child elements is dropped. A tree keeps its elements in one vector and
//! its names and text in one string, so reading a peripheral costs a few allocations, not one per
//! element (the TC39x file has 4.4 million).

use std::borrow::Cow;
use std::io::BufRead;

use quick_xml::events::{BytesStart, Event};

use crate::Error;

/// No element (end of a sibling chain, no children).
const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Span(u32, u32);

struct Node {
    tag: Span,
    text: Span,
    /// Range in [`Tree::attrs`].
    attrs: (u32, u32),
    first_child: u32,
    last_child: u32,
    next: u32,
}

/// An element and its descendants; the element is the tree's root (index 0).
#[derive(Default)]
pub(crate) struct Tree {
    nodes: Vec<Node>,
    strings: String,
    attrs: Vec<(Span, Span)>,
}

/// An element of a [`Tree`].
#[derive(Clone, Copy)]
pub(crate) struct El<'t> {
    t: &'t Tree,
    i: u32,
}

impl Tree {
    pub fn root(&self) -> El<'_> {
        El { t: self, i: 0 }
    }

    fn put(&mut self, s: &str) -> Span {
        let start = self.strings.len() as u32;
        self.strings.push_str(s);
        Span(start, s.len() as u32)
    }

    fn str(&self, s: Span) -> &str {
        &self.strings[s.0 as usize..(s.0 + s.1) as usize]
    }

    /// Adds an element under `parent` (`NONE`: the root); returns its index.
    fn add(&mut self, parent: u32, tag: &str, attrs: impl Iterator<Item = (String, String)>) -> u32 {
        let tag = self.put(tag);
        let a0 = self.attrs.len() as u32;
        for (k, v) in attrs {
            let kv = (self.put(&k), self.put(&v));
            self.attrs.push(kv);
        }
        let i = self.nodes.len() as u32;
        self.nodes.push(Node { tag, text: Span(0, 0), attrs: (a0, self.attrs.len() as u32), first_child: NONE, last_child: NONE, next: NONE });
        if parent != NONE {
            let p = &mut self.nodes[parent as usize];
            let prev = p.last_child;
            p.last_child = i;
            if prev == NONE {
                p.first_child = i;
            } else {
                self.nodes[prev as usize].next = i;
            }
        }
        i
    }

    /// Copies `src` and its descendants under `parent`.
    fn copy(&mut self, parent: u32, src: El) -> u32 {
        let i = self.add(parent, src.tag(), src.attrs().map(|(k, v)| (k.to_string(), v.to_string())));
        self.nodes[i as usize].text = self.put(src.text());
        for c in src.children() {
            self.copy(i, c);
        }
        i
    }

    /// A tree of its own for `src` and its descendants.
    pub fn copy_of(src: El) -> Tree {
        let mut t = Tree::default();
        t.copy(NONE, src);
        t
    }

    /// `{ ...base, ...over }` of two xml2js elements: each child tag, the attributes and the text
    /// come from `over` where it has them, else from `base`.
    pub fn merged(base: El, over: El) -> Tree {
        let mut t = Tree::default();
        let attrs = if over.attrs().next().is_some() { over } else { base };
        let root = t.add(NONE, over.tag(), attrs.attrs().map(|(k, v)| (k.to_string(), v.to_string())));
        t.nodes[root as usize].text = t.put(if over.text().is_empty() { base.text() } else { over.text() });
        for c in base.children().filter(|c| !over.has(c.tag())) {
            t.copy(root, c);
        }
        for c in over.children() {
            t.copy(root, c);
        }
        t
    }
}

impl<'t> El<'t> {
    fn node(&self) -> &'t Node {
        &self.t.nodes[self.i as usize]
    }

    pub fn tag(&self) -> &'t str {
        self.t.str(self.node().tag)
    }

    /// The text, for an element with only text (or none). Whitespace around child elements is
    /// dropped, as xml2js drops it.
    pub fn text(&self) -> &'t str {
        self.t.str(self.node().text)
    }

    pub fn attrs(&self) -> impl Iterator<Item = (&'t str, &'t str)> + 't {
        let t = self.t;
        let (a, b) = self.node().attrs;
        t.attrs[a as usize..b as usize].iter().map(move |&(k, v)| (t.str(k), t.str(v)))
    }

    pub fn attr(&self, name: &str) -> Option<&'t str> {
        self.attrs().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    pub fn children(&self) -> impl Iterator<Item = El<'t>> + 't {
        let t = self.t;
        let mut i = self.node().first_child;
        std::iter::from_fn(move || {
            (i != NONE).then(|| {
                let e = El { t, i };
                i = t.nodes[i as usize].next;
                e
            })
        })
    }

    pub fn first(&self, tag: &str) -> Option<El<'t>> {
        self.children().find(|c| c.tag() == tag)
    }

    pub fn all<'a>(&self, tag: &'a str) -> impl Iterator<Item = El<'t>> + 'a
    where
        't: 'a,
    {
        self.children().filter(move |c| c.tag() == tag)
    }

    pub fn has(&self, tag: &str) -> bool {
        self.first(tag).is_some()
    }

    /// The text of the first child `tag` (xml2js `e.tag[0]`).
    pub fn text_of(&self, tag: &str) -> Option<&'t str> {
        self.first(tag).map(|c| c.text())
    }
}

/// What the stream hands on.
pub(crate) enum Item {
    /// A child of `<device>` with a default (`name`, `size`, `access`, `resetValue`).
    Device(Tree),
    /// A `<peripheral>` of the device's first `<peripherals>`.
    Peripheral(Tree),
}

/// Device children the conversion reads.
const DEVICE_ITEMS: [&str; 4] = ["name", "size", "access", "resetValue"];

const ENTITIES: [(&str, &str); 5] = [("lt", "<"), ("gt", ">"), ("amp", "&"), ("quot", "\""), ("apos", "'")];

/// Decodes `&name;`, `&#N;` and `&#xN;`; an unknown entity stays as written.
pub(crate) fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        let end = after.find(';').filter(|&e| e > 0 && reference_ok(&after[..e]));
        let Some(e) = end else {
            out.push('&');
            rest = after;
            continue;
        };
        let r = &after[..e];
        let decoded = if let Some(n) = r.strip_prefix('#') {
            let code = match n.strip_prefix('x') {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => n.parse::<u32>().ok(),
            };
            code.and_then(char::from_u32).map(String::from)
        } else {
            ENTITIES.iter().find(|(k, _)| *k == r).map(|(_, v)| v.to_string())
        };
        match decoded {
            Some(d) => out.push_str(&d),
            None => {
                out.push('&');
                out.push_str(r);
                out.push(';');
            }
        }
        rest = &after[e + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The text between `&` and `;` of a reference as the TypeScript pattern matched it:
/// `#x<hex>`, `#<digits>` or `<letters>`.
fn reference_ok(r: &str) -> bool {
    if let Some(h) = r.strip_prefix("#x") {
        !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit())
    } else if let Some(d) = r.strip_prefix('#') {
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
    } else {
        r.bytes().all(|b| b.is_ascii_alphabetic())
    }
}

/// An element being read: its index and whether its text has CDATA. Its text collects in a
/// buffer of its depth, reused from element to element: most text is the whitespace between
/// child elements, which is dropped.
struct Open {
    i: u32,
    cdata: bool,
}

fn xml_err(e: impl std::fmt::Display) -> Error {
    Error::Xml(e.to_string())
}

fn attrs_of(e: &BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .with_checks(false)
        .filter_map(|a| a.ok())
        .map(|a| (String::from_utf8_lossy(a.key.as_ref()).into_owned(), decode_entities(&String::from_utf8_lossy(&a.value)).into_owned()))
        .collect()
}

/// Reads an SVD document, handing each [`Item`] to `on`. Returns whether the device has a
/// `<peripherals>` element. `on` may stop the stream with its error.
pub(crate) fn read<E: From<Error>>(source: impl BufRead, mut on: impl FnMut(Item) -> Result<(), E>) -> Result<bool, E> {
    let mut reader = quick_xml::Reader::from_reader(source);
    let config = reader.config_mut();
    config.trim_text(false);
    config.expand_empty_elements = false;
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut any = false;
    let mut tree = Tree::default();
    let mut stack: Vec<Open> = Vec::new();
    // The text of stack[i] is texts[i].
    let mut texts: Vec<String> = Vec::new();
    // In the device's first <peripherals>; whether one was seen.
    let (mut in_peripherals, mut seen_peripherals) = (false, false);
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| E::from(xml_err(e)))?;
        let (start, end) = match &event {
            Event::Start(_) => (true, false),
            Event::Empty(_) => (true, true),
            Event::End(_) => (false, true),
            Event::Text(t) => {
                if !stack.is_empty() {
                    texts[stack.len() - 1].push_str(&decode_entities(&String::from_utf8_lossy(t)));
                }
                (false, false)
            }
            Event::GeneralRef(r) => {
                if !stack.is_empty() {
                    texts[stack.len() - 1].push_str(&decode_entities(&format!("&{};", String::from_utf8_lossy(r))));
                }
                (false, false)
            }
            Event::CData(c) => {
                if let Some(top) = stack.last_mut() {
                    top.cdata = true;
                    texts[stack.len() - 1].push_str(&String::from_utf8_lossy(c));
                }
                (false, false)
            }
            Event::Eof => break,
            _ => (false, false),
        };
        if start {
            let e = match &event {
                Event::Start(e) | Event::Empty(e) => e,
                _ => unreachable!(),
            };
            let name = e.name();
            let tag = String::from_utf8_lossy(name.as_ref());
            depth += 1;
            any = true;
            if depth == 1 && tag != "device" {
                return Err(E::from(Error::Svd("Unable to parse SVD file: no device element".into())));
            }
            let capture = !stack.is_empty()
                || (depth == 2 && DEVICE_ITEMS.contains(&&*tag))
                || (depth == 3 && in_peripherals && tag == "peripheral");
            if capture {
                match texts.get_mut(stack.len()) {
                    Some(t) => t.clear(),
                    None => texts.push(String::new()),
                }
                let parent = stack.last().map_or(NONE, |o| o.i);
                let i = tree.add(parent, &tag, attrs_of(e).into_iter());
                stack.push(Open { i, cdata: false });
            } else if depth == 2 && tag == "peripherals" && !seen_peripherals {
                seen_peripherals = true;
                in_peripherals = !end;
            }
        }
        if end {
            if let Some(o) = stack.pop() {
                let text = &texts[stack.len()];
                let n = &tree.nodes[o.i as usize];
                let blank = !o.cdata && text.chars().all(char::is_whitespace);
                // A blank element without attributes or children has its whitespace as its value.
                let leaf = n.first_child == NONE && n.attrs.0 == n.attrs.1;
                if !blank || leaf {
                    let span = tree.put(text);
                    tree.nodes[o.i as usize].text = span;
                }
                if stack.is_empty() {
                    let done = std::mem::take(&mut tree);
                    on(if depth == 2 { Item::Device(done) } else { Item::Peripheral(done) })?;
                }
            } else if depth == 2 && in_peripherals {
                in_peripherals = false;
            }
            depth -= 1;
        }
        buf.clear();
    }
    if depth > 0 {
        return Err(E::from(Error::Xml("unexpected end of the document".into())));
    }
    if !any {
        return Err(E::from(Error::Xml("no root element".into())));
    }
    Ok(seen_peripherals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_as_the_extension_decoded_them() {
        assert_eq!(decode_entities("a &lt;b&gt; &amp;&#65;&#x42;"), "a <b> &AB");
        assert_eq!(decode_entities("&nbsp; & x; &#xZZ;"), "&nbsp; & x; &#xZZ;");
        assert_eq!(decode_entities("&;"), "&;");
    }

    #[test]
    fn peripherals_one_at_a_time() {
        let xml = r#"<?xml version="1.0"?>
<device><name>D</name><size>16</size>
  <peripherals>
    <peripheral derivedFrom="X"><name>A</name><description>  two
      lines </description><access /></peripheral>
    <peripheral><name>B&amp;C</name><x><![CDATA[ <raw> ]]></x></peripheral>
  </peripherals>
  <peripherals><peripheral><name>ignored</name></peripheral></peripherals>
</device>"#;
        let mut items = Vec::new();
        let had = read::<Error>(xml.as_bytes(), |i| {
            items.push(i);
            Ok(())
        })
        .unwrap();
        assert!(had);
        let names: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::Device(t) => format!("device {}={}", t.root().tag(), t.root().text()),
                Item::Peripheral(t) => format!("peripheral {}", t.root().text_of("name").unwrap()),
            })
            .collect();
        assert_eq!(names, ["device name=D", "device size=16", "peripheral A", "peripheral B&C"]);
        let Item::Peripheral(a) = &items[2] else { panic!() };
        let a = a.root();
        assert_eq!(a.attr("derivedFrom"), Some("X"));
        assert_eq!(a.text_of("description"), Some("  two\n      lines "));
        assert_eq!(a.text_of("access"), Some(""));
        assert!(a.text().is_empty(), "whitespace between children is dropped");
        let Item::Peripheral(b) = &items[3] else { panic!() };
        assert_eq!(b.root().text_of("x"), Some(" <raw> "));

        // Merging as `{ ...base, ...over }`: tags of `over` replace those of `base`.
        let m = Tree::merged(b.root(), a);
        let m = m.root();
        assert_eq!((m.text_of("name"), m.text_of("x"), m.attr("derivedFrom")), (Some("A"), Some(" <raw> "), Some("X")));
        assert_eq!(m.children().count(), 4);
    }

    #[test]
    fn errors() {
        let r = read::<Error>("<other/>".as_bytes(), |_| Ok(()));
        assert!(matches!(r, Err(Error::Svd(_))));
        let r = read::<Error>("<device><a></b></device>".as_bytes(), |_| Ok(()));
        assert!(matches!(r, Err(Error::Xml(_))));
    }
}
