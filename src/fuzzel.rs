// Showing a list of rows in fuzzel and reading back the pick.

use crate::config::Config;
use crate::fail;
use crate::rows::{lines, pad, Row};
use crate::runtime::Runtime;
use std::io::Write;
use std::process::{Command, Stdio};
use std::{env, fs};

/// Keywords are appended to the row text at this column, past the right edge
/// of any sane fuzzel window, so they're matched but not seen. fuzzel only
/// ranks results (and highlights matches) when it matches the displayed text:
/// hiding keywords in a --match-nth column instead left results in input
/// order, so "bt" put Reboot above Bluetooth. Cost: fuzzel draws "…" at the
/// right edge of rows that have keywords.
const KEYWORD_COLUMN: usize = 150;

pub enum Pick {
    Chosen(usize),
    Cancel,
    Flip,
}

pub fn pick(cfg: &Config, rt: &Runtime, prompt: &str, rows: &[Row], visible: usize) -> Pick {
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
    for (r, line) in rows.iter().zip(lines(rows)) {
        // --index reports the original input position even after fuzzel
        // re-sorts the matches, so rows[i] is always the row picked.
        if r.keywords.is_empty() {
            input.push_str(&line);
        } else {
            input.push_str(&pad(&line, KEYWORD_COLUMN));
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
