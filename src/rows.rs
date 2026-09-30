// Items to the rows fuzzel shows, with states and details applied.

use crate::config::{Config, Item, Kind, Page};
use crate::states::States;
use std::collections::HashSet;

/// Width the name of a group / page-link row is padded to before its "▸".
const GROUP_WIDTH: usize = 12;
/// Width a leaf's "icon label" is padded to before its `detail` column.
const DETAIL_WIDTH: usize = 22;
/// Hints and details are joined to the row with no-break spaces (U+00A0),
/// which look the same. fuzzel's tie-break prefers a match that starts a word,
/// and it only counts real spaces as word starts, so a label or keyword match
/// beats one in the side text: "bt" finds Bluetooth before a device called
/// "BT20 Pro". Without this the side text wins, since it's further left than
/// the keywords.
const NBSP: char = '\u{a0}';

#[derive(Clone)]
pub enum Action {
    Run(String),
    Page(usize),
    Group(String, Vec<Item>),
}

#[derive(Clone)]
pub struct Row {
    pub text: String,
    /// A leaf's right-hand column; aligned per screen by `lines`.
    pub detail: Option<String>,
    pub keywords: String,
    pub action: Action,
}

impl Row {
    fn key(&self) -> Option<String> {
        match &self.action {
            Action::Run(r) => Some(format!("{}\0{r}", self.text)),
            _ => None,
        }
    }
}

/// The rows as displayed: details start in one column, just past the longest
/// label that has one (never before DETAIL_WIDTH), so they line up per screen.
pub fn lines(rows: &[Row]) -> Vec<String> {
    let width = rows
        .iter()
        .filter(|r| r.detail.is_some())
        .map(|r| r.text.chars().count() + 2)
        .max()
        .unwrap_or(0)
        .max(DETAIL_WIDTH);
    rows.iter()
        .map(|r| match &r.detail {
            Some(d) => format!("{}{}", pad(&r.text, width - 1), side_text(d)),
            None => r.text.clone(),
        })
        .collect()
}

pub fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        format!("{s} ")
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// A hint or detail with its leading gap, spaced with NBSPs (see [`NBSP`]).
fn side_text(s: &str) -> String {
    std::iter::once(NBSP).chain(s.chars().map(|c| if c == ' ' { NBSP } else { c })).collect()
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

fn resolve(it: &Item, st: &States, pages: &[Page]) -> Option<Row> {
    let (mut icon, mut label, mut keywords, mut hint) = (it.icon.clone(), it.label.clone(), it.keywords.clone(), it.hint.clone());
    let mut run = None;
    if let Kind::Leaf { run: r, state, states } = &it.kind {
        run = r.clone();
        let variant = state.as_deref().and_then(|cmd| states.get(st.get(cmd)).or_else(|| states.get("*")));
        if let Some(v) = variant {
            if let Some(x) = &v.icon { icon = x.clone() }
            if let Some(x) = &v.label { label = x.clone() }
            if let Some(x) = &v.run { run = Some(x.clone()) }
            if let Some(x) = &v.keywords { keywords = x.clone() }
            if let Some(x) = &v.hint { hint = x.clone() }
        }
    }
    let lead = if icon.is_empty() { String::new() } else { format!("{icon} ") };
    // Group / link rows show their live detail, when they have one, in place
    // of the fixed hint.
    let detail = it.detail.as_deref().map(|c| st.get(c)).filter(|d| !d.is_empty());
    let side = detail.unwrap_or(&hint);
    let nav = |name: &str| format!("{lead}{}▸ {}", pad(name, GROUP_WIDTH), side_text(side)).trim_end().to_string();

    let (text, action) = match &it.kind {
        Kind::Group(sub) => (nav(&label), Action::Group(label.clone(), sub.clone())),
        Kind::Link(p) => {
            let idx = pages.iter().position(|pg| pg.name.eq_ignore_ascii_case(p))?;
            (nav(&label), Action::Page(idx))
        }
        Kind::Leaf { .. } => (format!("{lead}{label}"), Action::Run(run?)),
        Kind::Use(_) => return None, // replaced at load; one left over was an error
    };
    let detail = match action {
        Action::Run(_) => detail.map(clean),
        _ => None, // nav rows already show theirs in place of the hint
    };
    Some(Row { text: clean(&text), detail, keywords: clean(&keywords), action })
}

pub fn rows(items: &[Item], st: &States, pages: &[Page]) -> Vec<Row> {
    items.iter().filter_map(|it| resolve(it, st, pages)).collect()
}

/// Every runnable entry, depth-first through all pages and groups.
pub fn all_leaves(cfg: &Config, st: &States) -> Vec<Row> {
    fn walk(items: &[Item], st: &States, pages: &[Page], out: &mut Vec<Row>) {
        for it in items {
            let Some(r) = resolve(it, st, pages) else { continue };
            match &r.action {
                Action::Run(_) => out.push(r),
                Action::Group(_, sub) => walk(sub, st, pages, out),
                Action::Page(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    for pg in &cfg.pages {
        walk(&pg.items, st, &cfg.pages, &mut out);
    }
    out
}

/// The visible rows, then the hidden, search-only tail: every other leaf,
/// once. fuzzel shows only `--lines` rows until something is typed.
pub fn with_tail(visible: Vec<Row>, leaves: &[Row]) -> Vec<Row> {
    let mut seen: HashSet<String> = visible.iter().filter_map(Row::key).collect();
    let mut all = visible;
    for l in leaves {
        if let Some(k) = l.key() {
            if seen.insert(k) {
                all.push(l.clone());
            }
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;

    const MENU: &str = r#"
        page "Main" {
            item "Wi-Fi" icon="W" keywords="wlan" detail="ssid" {
                state "radio"
                when "on" label="Wi-Fi off" run="off"
                when "*" run="on"
            }
            group "Power" hint="sleep, reboot" { item "Sleep" run="suspend"; item "Reboot" run="reboot" }
            link "Tools" page="Tools" detail="load"
        }
        page "Tools" { item "Top" run="btop"; item "Sleep" run="suspend" }
    "#;

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn states_pick_a_variant_or_the_fallback() {
        let (cfg, _) = parse_config(MENU).unwrap();
        let on = rows(&cfg.pages[0].items, &States::fixed(&[("radio", "on")]), &cfg.pages);
        assert!(matches!(&on[0].action, Action::Run(r) if r == "off"));
        assert_eq!((on[0].text.as_str(), on[0].keywords.as_str()), ("W Wi-Fi off", "wlan"));

        let other = rows(&cfg.pages[0].items, &States::fixed(&[("radio", "??")]), &cfg.pages);
        assert!(matches!(&other[0].action, Action::Run(r) if r == "on"));
        assert_eq!(other[0].text, "W Wi-Fi");
    }

    #[test]
    fn nav_rows_show_detail_in_place_of_hint() {
        let (cfg, _) = parse_config(MENU).unwrap();
        let r = rows(&cfg.pages[0].items, &States::fixed(&[("load", "0.42"), ("ssid", "home")]), &cfg.pages);
        assert_eq!(texts(&r)[1..], ["Power       ▸ \u{a0}sleep,\u{a0}reboot", "Tools       ▸ \u{a0}0.42"]);
        assert!(matches!(r[2].action, Action::Page(1)));
        assert_eq!(r[0].detail.as_deref(), Some("home"));
        assert_eq!(r[1].detail, None);
    }

    #[test]
    fn details_align_per_screen() {
        let row = |text: &str, detail: Option<&str>| Row { text: text.into(), detail: detail.map(Into::into), keywords: String::new(), action: Action::Run(String::new()) };
        let short = lines(&[row("a", Some("1")), row("b", None)]);
        assert_eq!(short, [format!("a{}\u{a0}1", " ".repeat(DETAIL_WIDTH - 2)), "b".into()]);
        let long = "x".repeat(30);
        let out = lines(&[row(&long, Some("1")), row("a", Some("2"))]);
        assert_eq!(out[0], format!("{long} \u{a0}1"));
        assert_eq!(out[1], format!("a{}\u{a0}2", " ".repeat(30)));
    }

    #[test]
    fn tail_adds_each_other_leaf_once() {
        let (cfg, _) = parse_config(MENU).unwrap();
        let st = States::fixed(&[]);
        let visible = rows(&cfg.pages[1].items, &st, &cfg.pages);
        let all = with_tail(visible, &all_leaves(&cfg, &st));
        assert_eq!(texts(&all), ["Top", "Sleep", "W Wi-Fi", "Reboot"]);
    }
}
