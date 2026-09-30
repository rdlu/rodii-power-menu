//! rodii-power-menu — a browsable, searchable power/system menu for niri,
//! shown with fuzzel and described by a KDL file (see README.md for the format).
//!
//! Speed is the point: the parsed menu is cached in compiled form (the KDL
//! parser only runs after an edit), and every `state`/`detail` shell command
//! runs in parallel under a time budget. A check that misses the budget shows
//! its last known value (cached in $XDG_RUNTIME_DIR) and refreshes the cache in
//! the background for next time.

mod compiled;
mod config;
mod fuzzel;
mod parse;
mod rows;
mod runtime;
mod states;

use config::{config_path, load_config, Config, Item};
use fuzzel::{pick, Pick};
use rows::{all_leaves, lines, rows, with_tail, Action};
use runtime::Runtime;
use states::States;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs};

const NAME: &str = "rodii-power-menu";

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
        validate(&file);
        return;
    }
    let print_mode = cmd == Some("print");

    fix_env();
    let rt = Runtime::new();
    if !print_mode && rt.hand_off() {
        return;
    }

    let t0 = Instant::now();
    let (cfg, errs, compiled) = match compiled::load(&file, &rt) {
        Ok(c) => c,
        Err(e) => fail(&e),
    };
    if !errs.is_empty() {
        let list: Vec<String> = errs.iter().map(ToString::to_string).collect();
        let msg = format!("{} problem(s) in {} — run `{NAME} validate`\n{}", errs.len(), file.display(), list.join("\n"));
        if print_mode { eprintln!("{msg}") } else { notify(&msg) }
    }

    let t_parse = t0.elapsed();
    let mut st = States::start(&cfg, rt.cache());
    let t_states = t0.elapsed() - t_parse;

    if print_mode {
        print_pages(&cfg, &st);
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
    let cmd = run_menu(&cfg, &rt, &mut st);
    st.settle(Duration::ZERO);
    rt.release();
    if let Some(cmd) = cmd {
        let err = Command::new("sh").arg("-c").arg(&cmd).exec();
        fail(&format!("can't run {cmd:?}: {err}"));
    }
}

/// `validate`: report the config's problems; exits 1 if there are any.
fn validate(file: &Path) {
    match load_config(file) {
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
}

/// `print`, a debug/timing aid: every page as fuzzel would receive it.
fn print_pages(cfg: &Config, st: &States) {
    let leaves = all_leaves(cfg, st);
    for pg in &cfg.pages {
        let vis = rows(&pg.items, st, &cfg.pages);
        println!("=== {} (visible {})", pg.name, vis.len());
        let all = with_tail(vis, &leaves);
        for (r, line) in all.iter().zip(lines(&all)) {
            println!("{line}\t[{}]", r.keywords);
        }
    }
}

/// The menu: pages, groups and page flips until something is picked (its
/// command is returned) or the menu is closed.
fn run_menu(cfg: &Config, rt: &Runtime, st: &mut States) -> Option<String> {
    let mut page = 0usize;
    let mut stack: Vec<(String, Vec<Item>)> = Vec::new();
    loop {
        // Checks that missed the budget may have landed while fuzzel was up.
        st.settle(Duration::ZERO);
        let (title, list, visible) = match stack.last() {
            Some((title, items)) => {
                let r = rows(items, st, &cfg.pages);
                let n = r.len();
                (title.clone(), r, n)
            }
            None => {
                let pg = &cfg.pages[page];
                let r = rows(&pg.items, st, &cfg.pages);
                let n = r.len();
                (pg.name.clone(), with_tail(r, &all_leaves(cfg, st)), n)
            }
        };
        match pick(cfg, rt, &title, &list, visible) {
            Pick::Flip => {
                page = (page + 1) % cfg.pages.len();
                stack.clear();
            }
            // Esc backs out of a group; out of the page, it closes the menu.
            Pick::Cancel => {
                stack.pop()?;
            }
            Pick::Chosen(i) => match &list[i].action {
                Action::Run(c) => return Some(c.clone()),
                Action::Page(p) => {
                    page = *p;
                    stack.clear();
                }
                Action::Group(t, items) => stack.push((t.clone(), items.clone())),
            },
        }
    }
}
