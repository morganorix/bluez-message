//! Shared locations: iPhone address (`~/.config/bluez-message/device`) and daemon socket.

use std::path::{Path, PathBuf};

use crate::addr::BtAddr;
use crate::error::Error;

/// XDG `var` directory if absolute, otherwise `~/{fallback}`; a relative or empty `HOME` is
/// refused (the data would land in the working directory).
fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()).map(|h| h.join(fallback)))
}

/// `$XDG_CONFIG_HOME` or `~/.config`.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME` or `~/.local/share`.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Address given as argument, otherwise read from `~/.config/bluez-message/device`; validated.
/// `None`: no device configured yet (missing file).
///
/// # Errors
/// `HOME` missing, unreadable file or invalid address.
pub fn resolve_addr(arg: Option<BtAddr>) -> crate::error::Result<Option<BtAddr>> {
    if arg.is_some() {
        return Ok(arg);
    }
    let p = config_dir().ok_or(Error::NoHome)?.join("bluez-message/device");
    match std::fs::read_to_string(&p) {
        Ok(raw) => raw.trim().parse().map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Daemon socket in `$XDG_RUNTIME_DIR` (0700 directory owned by the user).
///
/// # Errors
/// [`Error::NoRuntimeDir`] if `XDG_RUNTIME_DIR` is missing or relative.
pub fn socket_path() -> crate::error::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
    dir.map(|d| d.join("bluez-message.sock")).ok_or(Error::NoRuntimeDir)
}

/// Window instance lock, next to the daemon socket: held while a window is open.
///
/// # Errors
/// As [`socket_path`].
pub fn gui_lock_path() -> crate::error::Result<PathBuf> {
    socket_path().map(|s| s.with_file_name("bluez-message-gui.lock"))
}

/// Saves the iPhone address to `~/.config/bluez-message/device` (atomic write, 0600).
///
/// # Errors
/// `HOME` missing or write impossible.
pub fn write_device(addr: &BtAddr) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| std::io::Error::other("HOME not set"))?.join("bluez-message");
    write_private(&dir.join("device"), format!("{addr}\n").as_bytes())
}

/// Atomic write of a private file: directory (specific to bluez-message) created or tightened to 0700, temporary
/// file 0600 synced then renamed.
///
/// # Errors
/// Directory creation, write, sync or rename impossible.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other("invalid path"));
    };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // Name unique to the write (process + counter): two concurrent writes (daemon and GUI) do not
    // delete each other's temporary file.
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let tmp = dir.join(tmp_name);
    // Leftover of an interrupted write (or planted link): removed, then exclusive creation without following links,
    // so that the file is really new and 0600.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    let r = f.write_all(bytes).and_then(|()| f.sync_all()).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    // The rename itself is durable only once the directory entry is synced.
    r.and_then(|()| std::fs::File::open(dir)?.sync_all())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_private_ignores_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let path = dir.path().join("sub/device");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::set_permissions(dir.path().join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path().join("sub")).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn write_private_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device");
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| (0..50).for_each(|_| write_private(&path, b"x").unwrap()));
            }
        });
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left");
    }
}
