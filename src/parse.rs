// KDL nodes to menu items, collecting problems as it goes.

use crate::config::{Item, Kind, Problem, Variant};
use kdl::{KdlDocument, KdlNode, KdlValue};
use std::collections::{BTreeMap, HashMap};

pub fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Strings as-is; numbers and booleans spelled out (so `when 1` = `when "1"`).
pub fn value_str(v: &KdlValue) -> String {
    match v {
        KdlValue::String(s) => s.clone(),
        KdlValue::Integer(i) => i.to_string(),
        KdlValue::Float(f) => f.to_string(),
        KdlValue::Bool(b) => b.to_string(),
        KdlValue::Null => String::new(),
    }
}

pub struct Parser<'a> {
    text: &'a str,
    pub errs: Vec<Problem>,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Self {
        Parser { text, errs: Vec::new() }
    }

    fn line(&self, n: &KdlNode) -> usize {
        line_of(self.text, n.span().offset())
    }

    pub fn err(&mut self, n: &KdlNode, msg: &str) {
        self.errs.push(Problem { line: self.line(n), msg: msg.to_string() });
    }

    /// The single positional argument (label / page name / id).
    pub fn label(&mut self, n: &KdlNode) -> Option<String> {
        let args: Vec<_> = n.entries().iter().filter(|e| e.name().is_none()).collect();
        match args.as_slice() {
            [one] => Some(value_str(one.value())),
            [] => {
                self.err(n, &format!("`{}` needs a name, e.g. `{} \"Name\"`", n.name().value(), n.name().value()));
                None
            }
            _ => {
                self.err(n, &format!("`{}` takes one name; quote it if it has spaces", n.name().value()));
                None
            }
        }
    }

    /// Named properties, checked against the node kind's allowed set.
    pub fn props(&mut self, n: &KdlNode, allowed: &[&str]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for e in n.entries() {
            let Some(k) = e.name() else { continue };
            let k = k.value();
            if allowed.contains(&k) {
                out.insert(k.to_string(), value_str(e.value()));
            } else {
                let hint = if allowed.is_empty() { "none allowed".to_string() } else { format!("allowed: {}", allowed.join(", ")) };
                self.err(n, &format!("`{}` has no property `{k}` ({hint})", n.name().value()));
            }
        }
        out
    }

    pub fn items(&mut self, doc: Option<&KdlDocument>) -> Vec<Item> {
        let Some(doc) = doc else { return Vec::new() };
        let mut out = Vec::new();
        for n in doc.nodes() {
            let it = match n.name().value() {
                "item" => self.item(n),
                "group" => {
                    let Some(label) = self.label(n) else { continue };
                    let mut pr = self.props(n, &["icon", "hint", "keywords", "id", "detail", "fresh"]);
                    let items = self.items(n.children());
                    if items.is_empty() {
                        self.err(n, &format!("group {label:?} has no items"));
                    }
                    Some(Item { kind: Kind::Group(items), ..self.base(n, label, &mut pr) })
                }
                "link" => {
                    let Some(label) = self.label(n) else { continue };
                    let mut pr = self.props(n, &["page", "icon", "hint", "keywords", "id", "detail", "fresh"]);
                    let Some(page) = pr.remove("page") else {
                        self.err(n, &format!("link {label:?} needs page=\"…\""));
                        continue;
                    };
                    self.no_children(n);
                    Some(Item { kind: Kind::Link(page), ..self.base(n, label, &mut pr) })
                }
                "use" => {
                    let Some(id) = self.label(n) else { continue };
                    self.props(n, &[]);
                    self.no_children(n);
                    Some(Item { line: self.line(n), kind: Kind::Use(id), ..Default::default() })
                }
                other => {
                    self.err(n, &format!("unknown node `{other}` (expected item, group, link, use)"));
                    None
                }
            };
            out.extend(it);
        }
        out
    }

    /// The fields item, group and link share, taken from their properties.
    fn base(&mut self, n: &KdlNode, label: String, pr: &mut HashMap<String, String>) -> Item {
        let detail_fresh = self.fresh(n, pr);
        Item {
            line: self.line(n),
            label,
            id: pr.remove("id"),
            icon: take(pr, "icon"),
            hint: take(pr, "hint"),
            keywords: take(pr, "keywords"),
            detail: pr.remove("detail"),
            detail_fresh,
            ..Default::default()
        }
    }

    fn item(&mut self, n: &KdlNode) -> Option<Item> {
        let label = self.label(n)?;
        let mut pr = self.props(n, &["icon", "run", "keywords", "id", "state", "detail", "fresh"]);
        let (mut run, mut state, mut states) = (pr.remove("run"), pr.remove("state"), BTreeMap::new());
        let mut it = self.base(n, label, &mut pr);
        // Long values can also be child nodes: `run "…"`, `state "…"`, …,
        // plus `when "<state output>" label=… run=…` variants.
        for c in n.children().map(KdlDocument::nodes).unwrap_or(&[]) {
            let ck = c.name().value();
            match ck {
                "run" | "state" | "detail" | "icon" | "keywords" => {
                    let Some(v) = self.label(c) else { continue };
                    let cp = self.props(c, if ck == "detail" { &["fresh"] } else { &[] });
                    if let Some(f) = cp.get("fresh") {
                        match f.as_str() {
                            "true" => it.detail_fresh = true,
                            "false" => {}
                            _ => self.err(c, "fresh takes #true or #false"),
                        }
                    }
                    match ck {
                        "run" => run = Some(v),
                        "state" => state = Some(v),
                        "detail" => it.detail = Some(v),
                        "icon" => it.icon = v,
                        _ => it.keywords = v,
                    }
                }
                "when" => {
                    let Some(key) = self.label(c) else { continue };
                    let mut vp = self.props(c, &["icon", "label", "run", "keywords", "hint"]);
                    if states.contains_key(&key) {
                        self.err(c, &format!("`when {key:?}` given twice"));
                    }
                    states.insert(key, Variant { icon: vp.remove("icon"), label: vp.remove("label"), run: vp.remove("run"), keywords: vp.remove("keywords"), hint: vp.remove("hint") });
                }
                other => self.err(c, &format!("unknown node `{other}` inside item (expected run, state, when, detail, icon, keywords)")),
            }
        }
        if !states.is_empty() && state.is_none() {
            self.err(n, &format!("item {:?} has `when` variants but no `state` command", it.label));
        }
        if state.is_some() && states.is_empty() {
            self.err(n, &format!("item {:?} has a `state` command but no `when` variants", it.label));
        }
        if run.is_none() && !states.values().any(|v| v.run.is_some()) {
            self.err(n, &format!("item {:?} needs run=\"…\" (or a run on its `when` variants)", it.label));
            return None;
        }
        Some(Item { kind: Kind::Leaf { run, state, states }, ..it })
    }

    /// The `fresh=#true|#false` property (it applies to the node's detail).
    fn fresh(&mut self, n: &KdlNode, pr: &mut HashMap<String, String>) -> bool {
        match pr.remove("fresh").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => {
                self.err(n, "fresh takes #true or #false");
                false
            }
        }
    }

    fn no_children(&mut self, n: &KdlNode) {
        if n.children().is_some_and(|c| !c.nodes().is_empty()) {
            self.err(n, &format!("`{}` takes no {{ … }} block", n.name().value()));
        }
    }
}

fn take(pr: &mut HashMap<String, String>, k: &str) -> String {
    pr.remove(k).unwrap_or_default()
}
