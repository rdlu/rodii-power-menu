// The parsed menu, "compiled" to a binary file in the runtime dir and reused
// while both the menu file and this binary are unchanged (size + mtime), so a
// normal open skips the KDL parser entirely.

use crate::config::{load_config, Config, Problem};
use crate::runtime::Runtime;
use std::path::Path;
use std::{env, fs};

/// The config, from the compiled cache when it's current. Also returns
/// whether it was a hit.
pub fn load(file: &Path, rt: &Runtime) -> Result<(Config, Vec<Problem>, bool), String> {
    let key = key(file);
    let path = rt.dir.join("menu.bin");
    if let (Some(key), Ok(bytes)) = (&key, fs::read(&path)) {
        if let Ok((k, cfg, errs)) = postcard::from_bytes::<(String, Config, Vec<Problem>)>(&bytes) {
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

fn key(file: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |p: &Path| fs::metadata(p).ok().map(|m| format!("{}:{}.{}", m.len(), m.mtime(), m.mtime_nsec()));
    let exe = env::current_exe().ok()?;
    Some(format!("{}|{}|{}|{}", env!("CARGO_PKG_VERSION"), stamp(&exe)?, file.display(), stamp(file)?))
}
