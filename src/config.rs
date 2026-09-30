// The menu is a KDL file (see README.md). Top-level nodes:
//   state-timeout-ms 40          budget for state/detail commands
//   fuzzel "--font=…" …          extra fuzzel arguments
//   page "Name" { …items… }      pages, in Mod+Escape order
//   define { …items… }           items only reachable through `use`
// Item nodes: item / group / link / use (see Parser::items).

use crate::parse::{line_of, value_str, Parser};
use crate::{home, NAME};
use kdl::{KdlDocument, KdlValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::{env, fs};

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub fuzzel: Vec<String>,
    pub state_timeout_ms: u64,
    pub pages: Vec<Page>,
}

#[derive(Serialize, Deserialize)]
pub struct Page {
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Item {
    pub line: usize,
    pub id: Option<String>,
    pub icon: String,
    pub label: String,
    pub keywords: String,
    pub hint: String,
    /// Shell command; its first output line is shown as a right column.
    pub detail: Option<String>,
    /// `detail "…" fresh=#true`: wait for it (within the budget) like a
    /// `state`, instead of showing the cached value, for details that change
    /// often and answer fast (e.g. the Wi-Fi network).
    pub detail_fresh: bool,
    pub kind: Kind,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Kind {
    /// A command to run.
    Leaf {
        run: Option<String>,
        /// Shell command; its first output line picks a variant from `states`.
        state: Option<String>,
        states: BTreeMap<String, Variant>,
    },
    /// A "▸" row that opens a submenu.
    Group(Vec<Item>),
    /// A "▸" row that goes to another page (by name).
    Link(String),
    /// A `use "id"` placeholder, replaced by the item with that id at load.
    Use(String),
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Leaf { run: None, state: None, states: BTreeMap::new() }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Variant {
    pub icon: Option<String>,
    pub label: Option<String>,
    pub run: Option<String>,
    pub keywords: Option<String>,
    pub hint: Option<String>,
}

/// A config problem that only drops the offending item.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// Calls `f` on every item, depth-first through groups.
pub fn visit<'a>(items: &'a [Item], f: &mut impl FnMut(&'a Item)) {
    for it in items {
        f(it);
        if let Kind::Group(sub) = &it.kind {
            visit(sub, f);
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Some(p) = env::var_os("RODII_POWER_MENU_CONFIG") {
        return p.into();
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join(NAME).join("menu.kdl")
}

/// Reads the file. Err = unusable (unreadable / not KDL / no pages); the Vec
/// holds problems that only drop the offending item.
pub fn load_config(path: &Path) -> Result<(Config, Vec<Problem>), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The file's text to a config; Err messages lack the file name.
pub fn parse_config(text: &str) -> Result<(Config, Vec<Problem>), String> {
    let doc: KdlDocument = text.parse().map_err(|e: kdl::KdlError| {
        let mut msg = "not valid KDL".to_string();
        for d in &e.diagnostics {
            let what = d.message.clone().or_else(|| d.label.clone()).unwrap_or_else(|| "syntax error".into());
            msg.push_str(&format!("\nline {}: {what}", line_of(text, d.span.offset())));
        }
        msg
    })?;

    let mut p = Parser::new(text);
    let mut cfg = Config { fuzzel: default_fuzzel(), state_timeout_ms: 40, pages: Vec::new() };
    let mut defines = Vec::new();
    for n in doc.nodes() {
        match n.name().value() {
            "state-timeout-ms" => match n.get(0).and_then(KdlValue::as_integer) {
                Some(ms) if ms >= 0 => cfg.state_timeout_ms = ms as u64,
                _ => p.err(n, "state-timeout-ms needs a number, e.g. `state-timeout-ms 40`"),
            },
            "fuzzel" => cfg.fuzzel = n.entries().iter().filter(|e| e.name().is_none()).map(|e| value_str(e.value())).collect(),
            "page" => {
                let Some(name) = p.label(n) else { continue };
                if cfg.pages.iter().any(|pg: &Page| pg.name.eq_ignore_ascii_case(&name)) {
                    p.err(n, &format!("page {name:?} defined twice"));
                }
                p.props(n, &[]);
                let items = p.items(n.children());
                cfg.pages.push(Page { name, items });
            }
            "define" => {
                p.props(n, &[]);
                defines.extend(p.items(n.children()));
            }
            other => p.err(n, &format!("unknown node `{other}` (expected page, define, fuzzel, state-timeout-ms)")),
        }
    }
    if cfg.pages.is_empty() {
        return Err("no `page` nodes".into());
    }

    let mut errs = p.errs;
    resolve_uses(&mut cfg, &defines, &mut errs);
    check_links(&cfg, &mut errs);
    errs.sort_by_key(|e| e.line);
    errs.dedup();
    Ok((cfg, errs))
}

/// Collects every id (anywhere, any order), then replaces each `use` with a
/// copy of its target.
fn resolve_uses(cfg: &mut Config, defines: &[Item], errs: &mut Vec<Problem>) {
    let mut ids: HashMap<String, Item> = HashMap::new();
    let mut collect = |it: &Item| {
        let Some(id) = &it.id else { return };
        if let Some(prev) = ids.get(id) {
            errs.push(Problem { line: it.line, msg: format!("id {id:?} already used on line {}", prev.line) });
        } else {
            ids.insert(id.clone(), it.clone());
        }
    };
    visit(defines, &mut collect);
    for pg in &cfg.pages {
        visit(&pg.items, &mut collect);
    }

    fn expand(items: &[Item], ids: &HashMap<String, Item>, depth: usize, errs: &mut Vec<Problem>) -> Vec<Item> {
        let mut out = Vec::new();
        for it in items {
            let mut it = match &it.kind {
                Kind::Use(id) => match ids.get(id) {
                    Some(target) if depth < 8 => target.clone(),
                    Some(_) => {
                        errs.push(Problem { line: it.line, msg: format!("`use {id:?}` nests too deep (does the group use itself?)") });
                        continue;
                    }
                    None => {
                        errs.push(Problem { line: it.line, msg: format!("`use {id:?}`: no item has id={id:?}") });
                        continue;
                    }
                },
                _ => it.clone(),
            };
            if let Kind::Group(sub) = &mut it.kind {
                *sub = expand(sub, ids, depth + 1, errs);
            }
            out.push(it);
        }
        out
    }
    for pg in &mut cfg.pages {
        pg.items = expand(&pg.items, &ids, 0, errs);
    }
}

/// Links must point at real pages.
fn check_links(cfg: &Config, errs: &mut Vec<Problem>) {
    for pg in &cfg.pages {
        visit(&pg.items, &mut |it| {
            let Kind::Link(target) = &it.kind else { return };
            if !cfg.pages.iter().any(|p| p.name.eq_ignore_ascii_case(target)) {
                errs.push(Problem { line: it.line, msg: format!("link to page {target:?}, which doesn't exist") });
            }
        });
    }
}

fn default_fuzzel() -> Vec<String> {
    vec!["--font=JetBrainsMono Nerd Font Mono:size=14".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> (Config, Vec<String>) {
        let (cfg, errs) = parse_config(text).unwrap_or_else(|e| panic!("{e}"));
        (cfg, errs.iter().map(ToString::to_string).collect())
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|it| it.label.as_str()).collect()
    }

    #[test]
    fn top_level_settings_and_kinds() {
        let (cfg, errs) = load(
            r#"
            state-timeout-ms 90
            fuzzel "--width=40"
            page "Main" {
                item "Lock" run="loginctl lock-session"
                group "More" { item "Sleep" run="systemctl suspend" }
                link "Tools" page="tools"
            }
            page "Tools" { item "Top" run="btop" }
            "#,
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.state_timeout_ms, 90);
        assert_eq!(cfg.fuzzel, ["--width=40"]);
        let main = &cfg.pages[0].items;
        assert_eq!(labels(main), ["Lock", "More", "Tools"]);
        assert!(matches!(&main[0].kind, Kind::Leaf { run: Some(r), .. } if r == "loginctl lock-session"));
        assert!(matches!(&main[1].kind, Kind::Group(sub) if labels(sub) == ["Sleep"]));
        assert!(matches!(&main[2].kind, Kind::Link(p) if p == "tools"));
    }

    #[test]
    fn use_copies_items_from_anywhere() {
        let (cfg, errs) = load(
            r#"
            page "A" { use "g"; use "x" }
            page "B" { item "X" id="x" run="x" }
            define { group "G" id="g" { use "x" } }
            "#,
        );
        assert!(errs.is_empty(), "{errs:?}");
        let a = &cfg.pages[0].items;
        assert_eq!(labels(a), ["G", "X"]);
        assert!(matches!(&a[0].kind, Kind::Group(sub) if labels(sub) == ["X"]));
    }

    #[test]
    fn use_problems() {
        let (cfg, errs) = load(
            r#"
            page "A" {
                use "nope"
                group "Loop" id="loop" { use "loop" }
                item "One" id="d" run="1"
                item "Two" id="d" run="2"
            }
            "#,
        );
        assert_eq!(errs, [
            r#"line 3: `use "nope"`: no item has id="nope""#,
            r#"line 4: `use "loop"` nests too deep (does the group use itself?)"#,
            r#"line 6: id "d" already used on line 5"#,
        ]);
        assert_eq!(labels(&cfg.pages[0].items), ["Loop", "One", "Two"]);
    }

    #[test]
    fn links_must_name_a_page() {
        let (_, errs) = load("page \"A\" {\n  link \"B\" page=\"b\"\n  link \"C\" page=\"c\"\n}\npage \"B\" { }");
        assert_eq!(errs, [r#"line 3: link to page "c", which doesn't exist"#]);
    }

    #[test]
    fn child_nodes_and_variants() {
        let (cfg, errs) = load(
            r#"
            page "A" {
                item "Wi-Fi" icon="x" {
                    state "nmcli radio wifi"
                    detail "iwgetid -r" fresh=#true
                    icon "y"
                    when "enabled" label="Wi-Fi off" run="nmcli radio wifi off"
                    when "*" run="nmcli radio wifi on"
                }
            }
            "#,
        );
        assert!(errs.is_empty(), "{errs:?}");
        let it = &cfg.pages[0].items[0];
        assert_eq!((it.icon.as_str(), it.detail.as_deref(), it.detail_fresh), ("y", Some("iwgetid -r"), true));
        let Kind::Leaf { run, state, states } = &it.kind else { panic!("not a leaf") };
        assert_eq!((run.as_deref(), state.as_deref()), (None, Some("nmcli radio wifi")));
        assert_eq!(states.keys().collect::<Vec<_>>(), ["*", "enabled"]);
        assert_eq!(states["enabled"].label.as_deref(), Some("Wi-Fi off"));
    }

    #[test]
    fn item_problems_are_sorted_by_line() {
        let (cfg, errs) = load("page \"A\" {\n  item \"s\" run=\"a\" state=\"b\"\n  item \"x\"\n  group \"G\" fresh=1\n}\nbogus");
        assert_eq!(errs, [
            r#"line 2: item "s" has a `state` command but no `when` variants"#,
            r#"line 3: item "x" needs run="…" (or a run on its `when` variants)"#,
            r#"line 4: group "G" has no items"#,
            "line 4: fresh takes #true or #false",
            "line 6: unknown node `bogus` (expected page, define, fuzzel, state-timeout-ms)",
        ]);
        assert_eq!(labels(&cfg.pages[0].items), ["s", "G"]);
    }

    #[test]
    fn unusable_files() {
        assert_eq!(parse_config("fuzzel \"--x\"").err().unwrap(), "no `page` nodes");
        assert!(parse_config("page \"A\" {").err().unwrap().starts_with("not valid KDL\nline 1"));
    }
}
