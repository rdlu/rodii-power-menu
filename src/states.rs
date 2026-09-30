// Parallel state/detail evaluation with a deadline and a cache fallback.

use crate::config::{visit, Config, Kind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub struct States {
    values: HashMap<String, String>,
    rx: Receiver<(String, String)>,
    pub pending: usize,
    cache: PathBuf,
}

impl States {
    pub fn start(cfg: &Config, cache: PathBuf) -> States {
        // `state` picks the action, so the menu waits for it (within the
        // budget). `detail` is only descriptive: once it has a cached value the
        // menu shows that at once and the check just refreshes the cache.
        let mut states = HashSet::new();
        let mut details = HashSet::new();
        for pg in &cfg.pages {
            visit(&pg.items, &mut |it| {
                if let Kind::Leaf { state: Some(cmd), .. } = &it.kind {
                    states.insert(cmd.clone());
                }
                // A fresh detail is waited for, exactly like a state.
                if it.detail_fresh {
                    states.extend(it.detail.clone());
                } else {
                    details.extend(it.detail.clone());
                }
            });
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

    /// Known outputs, no commands run (for tests).
    #[cfg(test)]
    pub fn fixed(values: &[(&str, &str)]) -> States {
        let (_, rx) = mpsc::channel();
        let values = values.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        States { values, rx, pending: 0, cache: PathBuf::new() }
    }

    pub fn get(&self, cmd: &str) -> &str {
        self.values.get(cmd).map(String::as_str).unwrap_or("")
    }

    /// Take in whatever finished late, and persist everything for next time.
    pub fn settle(&mut self, wait: Duration) {
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
    let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
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
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(c: &Command) -> Vec<String> {
        std::iter::once(c.get_program()).chain(c.get_args()).map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn plain_commands_skip_the_shell() {
        assert_eq!(argv(&shell_or_direct("nmcli  radio wifi")), ["nmcli", "radio", "wifi"]);
    }

    #[test]
    fn shell_syntax_uses_sh() {
        for cmd in ["a | b", "echo $HOME", "x=1 y", "a\nb", "ls ~", ""] {
            assert_eq!(argv(&shell_or_direct(cmd)), ["sh", "-c", cmd], "{cmd:?}");
        }
    }

    #[test]
    fn cache_round_trips_multiline_commands() {
        let dir = std::env::temp_dir().join(format!("rodii-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state-cache");
        let values = HashMap::from([("a\nb".to_string(), "on".to_string()), ("c".to_string(), String::new())]);
        write_cache(&path, &values);
        assert_eq!(read_cache(&path), values);
        let _ = std::fs::remove_dir_all(dir);
    }
}
