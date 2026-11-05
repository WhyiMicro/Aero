//! Tiny file logger: everything goes to %APPDATA%\Aero\aero.log.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(path: &Path) {
    if let Ok(f) = File::create(path) {
        let _ = LOG.set(Mutex::new(f));
    }
}

pub fn write(msg: String) {
    let line = format!("[{}] {}\n", chrono::Local::now().format("%H:%M:%S%.3f"), msg);
    #[cfg(debug_assertions)]
    eprint!("{line}");
    if let Some(f) = LOG.get()
        && let Ok(mut f) = f.lock()
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::write(format!($($t)*)) };
}
