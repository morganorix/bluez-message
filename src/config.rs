//! Emplacements partagés : adresse de l'iPhone (`~/.config/bluez-message/device`) et socket du démon.

use std::path::{Path, PathBuf};

use crate::addr::BtAddr;
use crate::t;

/// Dossier XDG `var` s'il est absolu, sinon `~/{fallback}`.
fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

/// `$XDG_CONFIG_HOME` ou `~/.config`.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME` ou `~/.local/share`.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Adresse donnée en argument, sinon lue dans `~/.config/bluez-message/device` ; validée.
///
/// # Errors
/// Message traduit : `HOME` absent, fichier illisible ou adresse invalide.
pub fn resolve_addr(arg: Option<BtAddr>) -> Result<BtAddr, String> {
    if let Some(a) = arg {
        return Ok(a);
    }
    let p = config_dir().ok_or(t!("HOME not set"))?.join("bluez-message/device");
    let raw = std::fs::read_to_string(&p).map_err(|e| t!("address missing (argument or {0}): {1}", p.display(), e))?;
    raw.trim().parse().map_err(|e: crate::error::Error| e.to_string())
}

/// Socket du démon dans `$XDG_RUNTIME_DIR` (répertoire 0700 propre à l'utilisateur).
///
/// # Errors
/// Message traduit si `XDG_RUNTIME_DIR` est absent ou relatif.
pub fn socket_path() -> Result<PathBuf, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
    dir.map(|d| d.join("bluez-message.sock")).ok_or_else(|| t!("XDG_RUNTIME_DIR missing or relative").to_owned())
}

/// Enregistre l'adresse de l'iPhone dans `~/.config/bluez-message/device` (écriture atomique, 0600).
///
/// # Errors
/// `HOME` absent ou écriture impossible.
pub fn write_device(addr: &BtAddr) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| std::io::Error::other("HOME not set"))?.join("bluez-message");
    write_private(&dir.join("device"), format!("{addr}\n").as_bytes())
}

/// Écriture atomique d'un fichier privé : dossier (propre à bluez-message) créé ou resserré en 0700, fichier
/// temporaire 0600 synchronisé puis renommé.
///
/// # Errors
/// Création du dossier, écriture, synchronisation ou renommage impossible.
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
    // Nom propre à l'écriture (processus + compteur) : deux écritures concurrentes (démon et GUI) ne
    // suppriment pas le temporaire l'une de l'autre.
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let tmp = dir.join(tmp_name);
    // Reste d'une écriture interrompue (ou lien planté) : retiré, puis création exclusive sans suivre de lien,
    // pour que le fichier soit bien neuf et en 0600.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    let r = f.write_all(bytes).and_then(|()| f.sync_all()).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
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
