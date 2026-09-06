//! Where this machine's long-term key lives, and how a peer's key is named on the command
//! line.
//!
//! The key is generated once and kept. Losing it is not a recoverable state — every peer that
//! ever paired with this machine pinned the public half, so a new key is a new machine as far
//! as any of them is concerned. That is the property pairing depends on, so the file is
//! written with an owner-only mode and never regenerated over an existing one.
//!
//! This is control plane rather than data plane, and it lives here anyway because the data
//! plane needs it: no session starts without an identity, and the Node-API surface can only
//! expose what Rust holds. Nothing here is on the frame path.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::net::handshake::{Identity, KEY_LEN};

/// Where the identity is kept when the command line does not say.
///
/// Under the user's home rather than beside the binary, because the key belongs to the
/// person running the host, not to the build.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] if there is no home directory to put it in.
pub fn default_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no home directory, so pass --identity with a path",
            )
        })?;

    Ok(Path::new(&home).join(".prism").join("identity.key"))
}

/// Loads the identity at `path`, generating and saving one if there is none.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be read or written, and
/// [`io::ErrorKind::InvalidData`] if it does not hold a thirty-two byte key in hex.
pub fn load_or_create(path: &Path) -> io::Result<Identity> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let bytes = from_hex(text.trim()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} does not hold a {KEY_LEN}-byte key in hex",
                        path.display()
                    ),
                )
            })?;

            Identity::from_private(&bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let identity = Identity::generate().map_err(|err| io::Error::other(err.to_string()))?;
            save(path, &identity)?;
            Ok(identity)
        }
        Err(err) => Err(err),
    }
}

/// Writes an identity to `path`, refusing to overwrite one that is already there.
///
/// # Errors
///
/// Returns [`io::ErrorKind::AlreadyExists`] if the file exists, and the underlying
/// [`io::Error`] if it cannot be written.
pub fn save(path: &Path, identity: &Identity) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Readable only by the owner, and set at creation rather than afterwards, so the key
        // is never briefly world-readable between the write and the chmod.
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    io::Write::write_all(&mut file, to_hex(identity.private()).as_bytes())?;
    io::Write::write_all(&mut file, b"\n")?;

    Ok(())
}

/// Parses a peer's public key from the hex the other side printed.
///
/// # Errors
///
/// Returns a message naming what was wrong, for the command line to print.
pub fn parse_peer_key(text: &str) -> Result<[u8; KEY_LEN], String> {
    from_hex(text.trim()).ok_or_else(|| {
        format!(
            "a peer key is {} hex characters, got {:?}",
            KEY_LEN * 2,
            text
        )
    })
}

/// Renders a key as lowercase hex, which is the form both sides paste.
#[must_use]
pub fn to_hex(key: &[u8; KEY_LEN]) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses lowercase or uppercase hex into a key, or `None` if it is not one.
fn from_hex(text: &str) -> Option<[u8; KEY_LEN]> {
    if text.len() != KEY_LEN * 2 {
        return None;
    }

    let mut key = [0u8; KEY_LEN];
    for (slot, pair) in key.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let digits = std::str::from_utf8(pair).ok()?;
        *slot = u8::from_str_radix(digits, 16).ok()?;
    }

    Some(key)
}

/// Where the keys of paired peers are kept, beside the identity.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] if there is no home directory.
pub fn default_peers_path() -> io::Result<PathBuf> {
    Ok(default_path()?.with_file_name("peers"))
}

/// Records a peer's public key, doing nothing if it is already there.
///
/// Append-only and one key per line. A machine may be paired with several peers, and
/// forgetting one because another was added later would mean re-pairing a machine that never
/// stopped being trusted.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be read or written.
pub fn remember_peer(path: &Path, key: &[u8; KEY_LEN]) -> io::Result<()> {
    let line = to_hex(key);

    if known_peers(path)?.iter().any(|known| known == key) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    io::Write::write_all(&mut file, line.as_bytes())?;
    io::Write::write_all(&mut file, b"\n")?;

    Ok(())
}

/// Reads every peer key that has been paired with, oldest first.
///
/// A missing file is an empty list rather than an error: a machine that has never paired has
/// no peers, which is a state and not a fault.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file exists but cannot be read, and
/// [`io::ErrorKind::InvalidData`] if a line is not a key.
pub fn known_peers(path: &Path) -> io::Result<Vec<[u8; KEY_LEN]>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            from_hex(line).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds a line that is not a key: {line:?}",
                        path.display()
                    ),
                )
            })
        })
        .collect()
}

/// Resolves the peer to talk to: the one named on the command line, or the last one paired.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] with an instruction to pair when neither is available,
/// and [`io::ErrorKind::InvalidInput`] if several peers are known and none was named.
pub fn resolve_peer(named: Option<&str>, peers_path: &Path) -> io::Result<[u8; KEY_LEN]> {
    if let Some(text) = named {
        return parse_peer_key(text)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err));
    }

    let known = known_peers(peers_path)?;

    match known.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no peer has been paired; run `prism-cli pair` on both machines, or pass --peer-key",
        )),
        1 => Ok(known[0]),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} peers are paired, so --peer-key has to say which:\n  {}",
                known.len(),
                known.iter().map(to_hex).collect::<Vec<_>>().join("\n  ")
            ),
        )),
    }
}
