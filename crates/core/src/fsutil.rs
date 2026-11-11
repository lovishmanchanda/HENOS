//! Small file helpers shared by the stores.

use std::{fs, io::Write, path::Path};

/// Writes `bytes` to `path` so that the file is readable only by the current
/// user and is never left half-written.
///
/// The data goes to a temp file created with owner-only permissions, which is
/// then renamed over the target.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    let _ = fs::remove_file(tmp);

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)
}
