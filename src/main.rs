//! rodii-power-menu — which-key style power/system menu for niri, shown with
//! fuzzel and described by a KDL file (see README.md for the format).
//!
//! Speed is the point: the parsed menu is cached in compiled form (the KDL
//! parser only runs after an edit), and every `state`/`detail` shell command
//! runs in parallel under a time budget. A check that misses the budget shows
//! its last known value (cached in $XDG_RUNTIME_DIR) and refreshes the cache in
//! the background for next time.

use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

const NAME: &str = "rodii-power-menu";
/// Width the name of a group / page-link row is padded to before its "▸".
const GROUP_WIDTH: usize = 12;
/// Width a leaf's "icon label" is padded to before its `detail` column.
const DETAIL_WIDTH: usize = 22;
/// Keywords are appended to the row text at this column, past the right edge
/// of any sane fuzzel window, so they're matched but not seen. fuzzel only
/// ranks results (and highlights matches) when it matches the displayed text:
/// hiding keywords in a --match-nth column instead left results in input
/// order, so "bt" put Reboot above Bluetooth. Cost: fuzzel draws "…" at the
/// right edge of rows that have keywords.
const KEYWORD_COLUMN: usize = 150;

// ---------------------------------------------------------------- config ---
//
// The menu is a KDL file (see README.md). Top-level nodes:
//   state-timeout-ms 40          budget for state/detail commands
//   fuzzel "--font=…" …          extra fuzzel arguments
//   page "Name" { …items… }      pages, in Mod+Escape order
//   define { …items… }           items only reachable through `use`
// Item nodes: item / group / link / use (see parse_items).

#[derive(Serialize, Deserialize)]
struct Config {
    fuzzel: Vec<String>,
    state_timeout_ms: u64,
    pages: Vec<Page>,
}

#[derive(Serialize, Deserialize)]
struct Page {
    name: String,
    items: Vec<Item>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Item {
    line: usize,
    id: Option<String>,
    /// A `use "id"` placeholder, replaced by the item with that id.
    uses: Option<String>,
    icon: String,
    label: String,
    run: Option<String>,
    keywords: String,
    hint: String,
    /// Makes this a group: a "▸" row that opens a submenu.
    items: Option<Vec<Item>>,
    /// Makes this a link to another page (by name).
    page: Option<String>,
    /// Shell command; its first output line picks a variant from `states`.
    state: Option<String>,
    states: BTreeMap<String, Variant>,
    /// Shell command; its first output line is shown as a right column.
    detail: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Variant {
    icon: Option<String>,
    label: Option<String>,
    run: Option<String>,
    keywords: Option<String>,
    hint: Option<String>,
}

fn config_path() -> PathBuf {
    if let Some(p) = env::var_os("RODII_POWER_MENU_CONFIG") {
        return p.into();
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join(NAME).join("menu.kdl")
}

/// Reads the file. Err = unusable (unreadable / not KDL / no pages); the Vec
/// holds problems that only drop the offending item, as "line N: message".
fn load_config(path: &Path) -> Result<(Config, Vec<String>), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: KdlDocument = text.parse().map_err(|e: kdl::KdlError| {
        let mut msg = format!("{}: not valid KDL", path.display());
        for d in &e.diagnostics {
            let what = d.message.clone().or_else(|| d.label.clone()).unwrap_or_else(|| "syntax error".into());
            msg.push_str(&format!("\nline {}: {what}", line_of(&text, d.span.offset())));
        }
        msg
    })?;

    let mut p = Parser { text: &text, errs: Vec::new() };
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
        return Err(format!("{}: no `page` nodes", path.display()));
    }

    // `use` resolution: collect every id (anywhere, any order), then replace
    // each `use` with a copy of its target.
    let mut ids: HashMap<String, Item> = HashMap::new();
    fn collect_ids(items: &[Item], ids: &mut HashMap<String, Item>, errs: &mut Vec<String>) {
        for it in items {
            if let Some(id) = &it.id {
                if let Some(prev) = ids.get(id) {
                    errs.push(format!("line {}: id {id:?} already used on line {}", it.line, prev.line));
                } else {
                    ids.insert(id.clone(), it.clone());
                }
            }
            if let Some(sub) = &it.items {
                collect_ids(sub, ids, errs);
            }
        }
    }
    collect_ids(&defines, &mut ids, &mut p.errs);
    for pg in &cfg.pages {
        collect_ids(&pg.items, &mut ids, &mut p.errs);
    }
    fn expand(items: &[Item], ids: &HashMap<String, Item>, depth: usize, errs: &mut Vec<String>) -> Vec<Item> {
        let mut out = Vec::new();
        for it in items {
            let mut it = match &it.uses {
                None => it.clone(),
                Some(id) => match ids.get(id) {
                    Some(target) if depth < 8 => target.clone(),
                    Some(_) => {
                        errs.push(format!("line {}: `use {id:?}` nests too deep (does the group use itself?)", it.line));
                        continue;
                    }
                    None => {
                        errs.push(format!("line {}: `use {id:?}`: no item has id={id:?}", it.line));
                        continue;
                    }
                },
            };
            if let Some(sub) = &it.items {
                it.items = Some(expand(sub, ids, depth + 1, errs));
            }
            out.push(it);
        }
        out
    }
    for pg in &mut cfg.pages {
        pg.items = expand(&pg.items, &ids, 0, &mut p.errs);
    }

    // Links must point at real pages.
    fn check_links(items: &[Item], pages: &[String], errs: &mut Vec<String>) {
        for it in items {
            if let Some(pg) = &it.page {
                if !pages.iter().any(|n| n.eq_ignore_ascii_case(pg)) {
                    errs.push(format!("line {}: link to page {pg:?}, which doesn't exist", it.line));
                }
            }
            if let Some(sub) = &it.items {
                check_links(sub, pages, errs);
            }
        }
    }
    let names: Vec<String> = cfg.pages.iter().map(|pg| pg.name.clone()).collect();
    for pg in &cfg.pages {
        check_links(&pg.items, &names, &mut p.errs);
    }

    let mut errs = p.errs;
    errs.sort_by_key(|e| e.split(':').next().and_then(|l| l.trim_start_matches("line ").parse::<usize>().ok()).unwrap_or(0));
    errs.dedup();
    Ok((cfg, errs))
}

/// The parsed menu, "compiled" to a binary file in the runtime dir and reused
/// while both the menu file and this binary are unchanged (size + mtime), so a
/// normal open skips the KDL parser entirely. Returns whether it was a hit.
fn load_compiled(file: &Path, rt: &Runtime) -> Result<(Config, Vec<String>, bool), String> {
    let key = compiled_key(file);
    let path = rt.dir.join("menu.bin");
    if let (Some(key), Ok(bytes)) = (&key, fs::read(&path)) {
        if let Ok((k, cfg, errs)) = postcard::from_bytes::<(String, Config, Vec<String>)>(&bytes) {
            if &k == key {
                return Ok((cfg, errs, true));
            }
        }
    }
    let (cfg, errs) = load_config(file)?;
    if let Some(key) = key {
        if let Ok(bytes) = postcard::to_allocvec(&(&key, &cfg, &errs)) {
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(tmp, &path);
            }
        }
    }
    Ok((cfg, errs, false))
}

fn compiled_key(file: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |p: &Path| fs::metadata(p).ok().map(|m| format!("{}:{}.{}", m.len(), m.mtime(), m.mtime_nsec()));
    let exe = env::current_exe().ok()?;
    Some(format!("{}|{}|{}|{}", env!("CARGO_PKG_VERSION"), stamp(&exe)?, file.display(), stamp(file)?))
}

fn default_fuzzel() -> Vec<String> {
    vec!["--font=JetBrainsMono Nerd Font Mono:size=14".into()]
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Strings as-is; numbers and booleans spelled out (so `when 1` = `when "1"`).
fn value_str(v: &KdlValue) -> String {
    match v {
        KdlValue::String(s) => s.clone(),
        KdlValue::Integer(i) => i.to_string(),
        KdlValue::Float(f) => f.to_string(),
        KdlValue::Bool(b) => b.to_string(),
        KdlValue::Null => String::new(),
    }
}

struct Parser<'a> {
    text: &'a str,
    errs: Vec<String>,
}

impl Parser<'_> {
    fn err(&mut self, n: &KdlNode, msg: &str) {
        self.errs.push(format!("line {}: {msg}", line_of(self.text, n.span().offset())));
    }

    /// The single positional argument (label / page name / id).
    fn label(&mut self, n: &KdlNode) -> Option<String> {
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
    fn props(&mut self, n: &KdlNode, allowed: &[&str]) -> HashMap<String, String> {
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

    fn items(&mut self, doc: Option<&KdlDocument>) -> Vec<Item> {
        let Some(doc) = doc else { return Vec::new() };
        let mut out = Vec::new();
        for n in doc.nodes() {
            let line = line_of(self.text, n.span().offset());
            let kind = n.name().value();
            let it = match kind {
                "item" => self.item(n, line),
                "group" => {
                    let Some(label) = self.label(n) else { continue };
                    let mut pr = self.props(n, &["icon", "hint", "keywords", "id"]);
                    let items = self.items(n.children());
                    if items.is_empty() {
                        self.err(n, &format!("group {label:?} has no items"));
                    }
                    Some(Item { line, label, id: pr.remove("id"), icon: take(&mut pr, "icon"), hint: take(&mut pr, "hint"), keywords: take(&mut pr, "keywords"), items: Some(items), ..Default::default() })
                }
                "link" => {
                    let Some(label) = self.label(n) else { continue };
                    let mut pr = self.props(n, &["page", "icon", "hint", "keywords", "id"]);
                    let Some(page) = pr.remove("page") else {
                        self.err(n, &format!("link {label:?} needs page=\"…\""));
                        continue;
                    };
                    self.no_children(n);
                    Some(Item { line, label, page: Some(page), id: pr.remove("id"), icon: take(&mut pr, "icon"), hint: take(&mut pr, "hint"), keywords: take(&mut pr, "keywords"), ..Default::default() })
                }
                "use" => {
                    let Some(id) = self.label(n) else { continue };
                    self.props(n, &[]);
                    self.no_children(n);
                    Some(Item { line, uses: Some(id), ..Default::default() })
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

    fn item(&mut self, n: &KdlNode, line: usize) -> Option<Item> {
        let label = self.label(n)?;
        let mut pr = self.props(n, &["icon", "run", "keywords", "id", "state", "detail"]);
        let mut it = Item { line, label, id: pr.remove("id"), icon: take(&mut pr, "icon"), run: pr.remove("run"), keywords: take(&mut pr, "keywords"), state: pr.remove("state"), detail: pr.remove("detail"), ..Default::default() };
        // Long values can also be child nodes: `run "…"`, `state "…"`, …,
        // plus `when "<state output>" label=… run=…` variants.
        for c in n.children().map(KdlDocument::nodes).unwrap_or(&[]) {
            let ck = c.name().value();
            match ck {
                "run" | "state" | "detail" | "icon" | "keywords" => {
                    let Some(v) = self.label(c) else { continue };
                    self.props(c, &[]);
                    let slot = match ck {
                        "run" => &mut it.run,
                        "state" => &mut it.state,
                        "detail" => &mut it.detail,
                        "icon" => { it.icon = v; continue; }
                        _ => { it.keywords = v; continue; }
                    };
                    *slot = Some(v);
                }
                "when" => {
                    let Some(key) = self.label(c) else { continue };
                    let mut vp = self.props(c, &["icon", "label", "run", "keywords", "hint"]);
                    if it.states.contains_key(&key) {
                        self.err(c, &format!("`when {key:?}` given twice"));
                    }
                    it.states.insert(key, Variant { icon: vp.remove("icon"), label: vp.remove("label"), run: vp.remove("run"), keywords: vp.remove("keywords"), hint: vp.remove("hint") });
                }
                other => self.err(c, &format!("unknown node `{other}` inside item (expected run, state, when, detail, icon, keywords)")),
            }
        }
        if !it.states.is_empty() && it.state.is_none() {
            self.err(n, &format!("item {:?} has `when` variants but no `state` command", it.label));
        }
        if it.state.is_some() && it.states.is_empty() {
            self.err(n, &format!("item {:?} has a `state` command but no `when` variants", it.label));
        }
        if it.run.is_none() && !it.states.values().any(|v| v.run.is_some()) {
            self.err(n, &format!("item {:?} needs run=\"…\" (or a run on its `when` variants)", it.label));
            return None;
        }
        Some(it)
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

// ------------------------------------------------------------ runtime dir ---

struct Runtime {
    dir: PathBuf,
}

impl Runtime {
    fn new() -> Runtime {
        let base = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(env::temp_dir);
        let dir = base.join(NAME);
        let _ = fs::create_dir_all(&dir);
        Runtime { dir }
    }
    fn pid(&self) -> PathBuf {
        self.dir.join("pid")
    }
    fn flip(&self) -> PathBuf {
        self.dir.join("flip")
    }
    fn fuzzel(&self) -> PathBuf {
        self.dir.join("fuzzel.pid")
    }
    fn cache(&self) -> PathBuf {
        self.dir.join("state-cache")
    }

    /// A second launch while the menu is open (Mod+Escape pressed again): tell
    /// the running instance to flip page by dropping a flag and closing its
    /// fuzzel. Returns true when that happened (and this instance should exit).
    fn hand_off(&self) -> bool {
        let Some(other) = read_pid(&self.pid()) else { return false };
        if other == std::process::id() || !is_us(other) {
            return false;
        }
        let _ = fs::write(self.flip(), b"");
        // The other instance may still be evaluating states and not have
        // started fuzzel yet; it checks the flag before starting it, so waiting
        // briefly for the pid file only matters for the narrow window between.
        for _ in 0..20 {
            if let Some(f) = read_pid(&self.fuzzel()) {
                unsafe { libc::kill(f as i32, libc::SIGTERM) };
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn claim(&self) {
        let _ = fs::write(self.pid(), std::process::id().to_string());
        let _ = fs::remove_file(self.flip());
    }

    fn take_flip(&self) -> bool {
        fs::remove_file(self.flip()).is_ok()
    }

    fn release(&self) {
        if read_pid(&self.pid()) == Some(std::process::id()) {
            let _ = fs::remove_file(self.pid());
        }
        let _ = fs::remove_file(self.flip());
        let _ = fs::remove_file(self.fuzzel());
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Is `pid` a running rodii-power-menu? (comm is truncated to 15 bytes.)
fn is_us(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|c| NAME.starts_with(c.trim()) && !c.trim().is_empty())
        .unwrap_or(false)
}

// ----------------------------------------------------------------- states ---

/// Parallel state/detail evaluation with a deadline and a cache fallback.
struct States {
    values: HashMap<String, String>,
    rx: Receiver<(String, String)>,
    pending: usize,
    cache: PathBuf,
}

impl States {
    fn start(cfg: &Config, cache: PathBuf) -> States {
        // `state` picks the action, so the menu waits for it (within the
        // budget). `detail` is only descriptive: once it has a cached value the
        // menu shows that at once and the check just refreshes the cache.
        let mut states = HashSet::new();
        let mut details = HashSet::new();
        fn collect(items: &[Item], states: &mut HashSet<String>, details: &mut HashSet<String>) {
            for it in items {
                states.extend(it.state.clone());
                details.extend(it.detail.clone());
                if let Some(sub) = &it.items {
                    collect(sub, states, details);
                }
            }
        }
        for pg in &cfg.pages {
            collect(&pg.items, &mut states, &mut details);
        }
        let values = read_cache(&cache);
        let mut waiting: HashSet<String> = states.clone();
        waiting.extend(details.iter().filter(|d| !values.contains_key(*d)).cloned());
        let cmds: Vec<String> = states.union(&details).cloned().collect();

        let (tx, rx) = mpsc::channel();
        for cmd in &cmds {
            let tx = tx.clone();
            let cmd = cmd.clone();
            thread::spawn(move || {
                let out = shell_or_direct(&cmd)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string())
                    .unwrap_or_default();
                let _ = tx.send((cmd, out));
            });
        }

        let mut st = States { values, rx, pending: cmds.len(), cache };
        let deadline = Instant::now() + Duration::from_millis(cfg.state_timeout_ms);
        while !waiting.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            match st.rx.recv_timeout(left) {
                Ok((c, v)) => {
                    waiting.remove(&c);
                    st.values.insert(c, v);
                    st.pending -= 1;
                }
                Err(_) => break, // budget spent: the rest show cached values
            }
        }
        st
    }

    fn get(&self, cmd: &str) -> &str {
        self.values.get(cmd).map(String::as_str).unwrap_or("")
    }

    /// Take in whatever finished late, and persist everything for next time.
    fn settle(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        while self.pending > 0 {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((c, v)) => {
                    self.values.insert(c, v);
                    self.pending -= 1;
                }
                Err(_) => break,
            }
        }
        write_cache(&self.cache, &self.values);
    }
}

/// `sh -c cmd`, or — when cmd has no shell syntax at all — the program run
/// directly, which saves the ~1 ms a shell costs to start.
fn shell_or_direct(cmd: &str) -> Command {
    const SHELLY: &[char] = &['|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '#', '~', '=', '%', '{', '}', '\n'];
    let words: Vec<&str> = cmd.split_whitespace().collect();
    if cmd.contains(SHELLY) || words.is_empty() {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    } else {
        let mut c = Command::new(words[0]);
        c.args(&words[1..]);
        c
    }
}

// Cache format: one entry per line, `command \x1f value`, newlines inside a
// (multi-line) command stored as \x1e.
fn read_cache(path: &Path) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| l.split_once('\x1f'))
        .map(|(k, v)| (k.replace('\x1e', "\n"), v.to_string()))
        .collect()
}

fn write_cache(path: &Path, values: &HashMap<String, String>) {
    let mut out = String::new();
    for (k, v) in values {
        out.push_str(&k.replace('\n', "\x1e"));
        out.push('\x1f');
        out.push_str(v);
        out.push('\n');
    }
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, out).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

// ------------------------------------------------------------------- rows ---

#[derive(Clone)]
enum Action {
    Run(String),
    Page(usize),
    Group(String, Vec<Item>),
}

#[derive(Clone)]
struct Row {
    text: String,
    keywords: String,
    action: Action,
}

impl Row {
    fn key(&self) -> Option<String> {
        match &self.action {
            Action::Run(r) => Some(format!("{}\0{r}", self.text)),
            _ => None,
        }
    }
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        format!("{s} ")
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

fn resolve(it: &Item, st: &States, pages: &[Page]) -> Option<Row> {
    let (mut icon, mut label, mut run, mut keywords, mut hint) =
        (it.icon.clone(), it.label.clone(), it.run.clone(), it.keywords.clone(), it.hint.clone());
    if let Some(cmd) = &it.state {
        let key = st.get(cmd);
        if let Some(v) = it.states.get(key).or_else(|| it.states.get("*")) {
            if let Some(x) = &v.icon { icon = x.clone() }
            if let Some(x) = &v.label { label = x.clone() }
            if let Some(x) = &v.run { run = Some(x.clone()) }
            if let Some(x) = &v.keywords { keywords = x.clone() }
            if let Some(x) = &v.hint { hint = x.clone() }
        }
    }
    let lead = if icon.is_empty() { String::new() } else { format!("{icon} ") };
    let nav = |name: &str| format!("{lead}{}▸  {hint}", pad(name, GROUP_WIDTH)).trim_end().to_string();

    let (text, action) = if let Some(sub) = &it.items {
        (nav(&label), Action::Group(label.clone(), sub.clone()))
    } else if let Some(p) = &it.page {
        let idx = pages.iter().position(|pg| pg.name.eq_ignore_ascii_case(p))?;
        (nav(&label), Action::Page(idx))
    } else {
        let run = run?;
        let mut text = format!("{lead}{label}");
        if let Some(d) = it.detail.as_deref().map(|c| st.get(c)).filter(|d| !d.is_empty()) {
            text = format!("{}{d}", pad(&text, DETAIL_WIDTH));
        }
        (text, Action::Run(run))
    };
    Some(Row { text: clean(&text), keywords: clean(&keywords), action })
}

fn rows(items: &[Item], st: &States, pages: &[Page]) -> Vec<Row> {
    items.iter().filter_map(|it| resolve(it, st, pages)).collect()
}

/// Every runnable entry, depth-first through all pages and groups.
fn all_leaves(cfg: &Config, st: &States) -> Vec<Row> {
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
fn with_tail(visible: Vec<Row>, leaves: &[Row]) -> Vec<Row> {
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

// ----------------------------------------------------------------- fuzzel ---

enum Pick {
    Chosen(usize),
    Cancel,
    Flip,
}

fn pick(cfg: &Config, rt: &Runtime, prompt: &str, rows: &[Row], visible: usize) -> Pick {
    if rt.take_flip() {
        return Pick::Flip;
    }
    let bin = env::var("RODII_FUZZEL").unwrap_or_else(|_| "fuzzel".into());
    let child = Command::new(&bin)
        .args(["--dmenu", "--index", "--minimal-lines"])
        .arg(format!("--prompt={prompt} › "))
        .arg(format!("--lines={}", visible.max(1)))
        .args(&cfg.fuzzel)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => fail(&format!("can't run {bin}: {e}")),
    };
    let _ = fs::write(rt.fuzzel(), child.id().to_string());

    let mut input = String::new();
    for r in rows {
        // --index reports the original input position even after fuzzel
        // re-sorts the matches, so rows[i] is always the row picked.
        if r.keywords.is_empty() {
            input.push_str(&r.text);
        } else {
            input.push_str(&pad(&r.text, KEYWORD_COLUMN));
            input.push_str(&r.keywords);
        }
        input.push('\n');
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child.wait_with_output();
    let _ = fs::remove_file(rt.fuzzel());

    if rt.take_flip() {
        return Pick::Flip;
    }
    let Ok(out) = out else { return Pick::Cancel };
    match String::from_utf8_lossy(&out.stdout).trim().parse::<usize>() {
        Ok(i) if i < rows.len() => Pick::Chosen(i),
        _ => Pick::Cancel,
    }
}

// ------------------------------------------------------------------- main ---

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

fn notify(msg: &str) {
    eprintln!("{NAME}: {msg}");
    let _ = Command::new("notify-send").args(["-a", NAME, "Power menu", msg]).status();
}

fn fail(msg: &str) -> ! {
    notify(msg);
    std::process::exit(1);
}

/// niri runs as a systemd user service without ~/.local/bin on PATH, and may
/// predate the ssh-agent socket; fix both for every command we start.
fn fix_env() {
    let local = home().join(".local/bin");
    let path = env::var("PATH").unwrap_or_default();
    if !path.split(':').any(|p| Path::new(p) == local) {
        env::set_var("PATH", format!("{}:{path}", local.display()));
    }
    let sock_ok = env::var_os("SSH_AUTH_SOCK").is_some_and(|s| is_socket(Path::new(&s)));
    if !sock_ok {
        if let Some(rt) = env::var_os("XDG_RUNTIME_DIR") {
            let fallback = Path::new(&rt).join("ssh-agent.socket");
            if is_socket(&fallback) {
                env::set_var("SSH_AUTH_SOCK", fallback);
            }
        }
    }
}

fn is_socket(p: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::metadata(p).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

const USAGE: &str = "\
usage: rodii-power-menu [COMMAND]

  (no command)       open the menu (bind this to Mod+Escape; run again while
                     it's open to flip to the next page)
  validate [FILE]    check the config, print problems with line numbers;
                     exit 1 if there are any (like `niri validate`)
  print [FILE]       show every page as fuzzel would get it, with live states
  -V, --version      print the version
  -h, --help         this help

config: $RODII_POWER_MENU_CONFIG, else $XDG_CONFIG_HOME/rodii-power-menu/menu.kdl";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);
    let file = args.get(1).map(PathBuf::from).unwrap_or_else(config_path);
    match cmd {
        None | Some("validate" | "print") => {}
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return;
        }
        Some("-V" | "--version") => {
            println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    }

    if cmd == Some("validate") {
        match load_config(&file) {
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
            Ok((cfg, errs)) if errs.is_empty() => {
                let n: usize = cfg.pages.iter().map(|p| p.items.len()).sum();
                println!("{}: config is valid ({} pages, {n} top-level items)", file.display(), cfg.pages.len());
            }
            Ok((_, errs)) => {
                eprintln!("{}:", file.display());
                for e in &errs {
                    eprintln!("  {e}");
                }
                std::process::exit(1);
            }
        }
        return;
    }
    let print_mode = cmd == Some("print");

    fix_env();
    let rt = Runtime::new();
    if !print_mode && rt.hand_off() {
        return;
    }

    let t0 = Instant::now();
    let (cfg, errs, compiled) = match load_compiled(&file, &rt) {
        Ok(c) => c,
        Err(e) => fail(&e),
    };
    if !errs.is_empty() {
        let msg = format!("{} problem(s) in {} — run `{NAME} validate`\n{}", errs.len(), file.display(), errs.join("\n"));
        if print_mode { eprintln!("{msg}") } else { notify(&msg) }
    }

    let t_parse = t0.elapsed();
    let mut st = States::start(&cfg, rt.cache());
    let t_states = t0.elapsed() - t_parse;

    if print_mode {
        let leaves = all_leaves(&cfg, &st);
        // Debug/timing aid: every page as fuzzel would receive it.
        for pg in &cfg.pages {
            let vis = rows(&pg.items, &st, &cfg.pages);
            println!("=== {} (visible {})", pg.name, vis.len());
            for r in with_tail(vis, &leaves) {
                println!("{}\t[{}]", r.text, r.keywords);
            }
        }
        println!(
            "--- config {} in {:.2} ms; state/detail checks {:.1} ms ({} still running at the {} ms budget)",
            if compiled { "loaded from the compiled cache" } else { "parsed (KDL) and compiled" },
            t_parse.as_secs_f64() * 1e3,
            t_states.as_secs_f64() * 1e3,
            st.pending,
            cfg.state_timeout_ms
        );
        st.settle(Duration::from_secs(2));
        return;
    }

    rt.claim();
    let mut page = 0usize;
    let mut stack: Vec<(String, Vec<Item>)> = Vec::new();
    let cmd = loop {
        // Checks that missed the budget may have landed while fuzzel was up.
        st.settle(Duration::ZERO);
        let (title, list, visible) = match stack.last() {
            Some((title, items)) => {
                let r = rows(items, &st, &cfg.pages);
                let n = r.len();
                (title.clone(), r, n)
            }
            None => {
                let pg = &cfg.pages[page];
                let r = rows(&pg.items, &st, &cfg.pages);
                let n = r.len();
                (pg.name.clone(), with_tail(r, &all_leaves(&cfg, &st)), n)
            }
        };
        match pick(&cfg, &rt, &title, &list, visible) {
            Pick::Flip => {
                page = (page + 1) % cfg.pages.len();
                stack.clear();
            }
            Pick::Cancel => {
                if stack.pop().is_none() {
                    break None;
                }
            }
            Pick::Chosen(i) => match &list[i].action {
                Action::Run(c) => break Some(c.clone()),
                Action::Page(p) => {
                    page = *p;
                    stack.clear();
                }
                Action::Group(t, items) => stack.push((t.clone(), items.clone())),
            },
        }
    };

    st.settle(Duration::ZERO);
    rt.release();
    if let Some(cmd) = cmd {
        let err = Command::new("sh").arg("-c").arg(&cmd).exec();
        fail(&format!("can't run {cmd:?}: {err}"));
    }
}
