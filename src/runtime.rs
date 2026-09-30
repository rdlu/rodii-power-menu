// Files in $XDG_RUNTIME_DIR/rodii-power-menu: the running instance's pid, its
// fuzzel's pid, the page-flip flag, and the caches.

use crate::NAME;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, thread};

pub struct Runtime {
    pub dir: PathBuf,
}

impl Runtime {
    pub fn new() -> Runtime {
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
    pub fn fuzzel(&self) -> PathBuf {
        self.dir.join("fuzzel.pid")
    }
    pub fn cache(&self) -> PathBuf {
        self.dir.join("state-cache")
    }

    /// A second launch while the menu is open (Mod+Escape pressed again): tell
    /// the running instance to flip page by dropping a flag and closing its
    /// fuzzel. Returns true when that happened (and this instance should exit).
    pub fn hand_off(&self) -> bool {
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

    pub fn claim(&self) {
        let _ = fs::write(self.pid(), std::process::id().to_string());
        let _ = fs::remove_file(self.flip());
    }

    pub fn take_flip(&self) -> bool {
        fs::remove_file(self.flip()).is_ok()
    }

    pub fn release(&self) {
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
