//! Replacing a file so that a machine losing power cannot lose what was in it.
//!
//! Three files in this project are the only copy of something: the account store, the sessions
//! beside it, and the list of machines this one will talk to. All three were written by putting
//! the new contents in a temporary file and renaming it over the old one, which is the right
//! shape — a rename is atomic, so a reader never sees half a file.
//!
//! It is not, on its own, enough. A rename being atomic says nothing about whether the bytes it
//! renames have reached the disk. A kernel is free to hold both the contents and the rename in
//! its cache and to write them in either order, so a machine that loses power between the two
//! comes back to a file that exists, is the right length, and is full of nothing. On a cheap
//! virtual server with write-back caching, that is a likelier way to lose an account store than
//! the machine itself failing.
//!
//! What closes it is two flushes: the contents before the rename, and the directory after it.
//! The first makes the temporary file real; the second makes the rename real.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Writes `bytes` to `path`, atomically, and does not return until the change is on the disk.
///
/// Creates the parent directory if it does not exist. The temporary file is left behind only if
/// the process dies mid-write, and is overwritten by the next attempt.
///
/// # Errors
///
/// Fails if the directory cannot be made, the temporary file cannot be written or flushed, or
/// the rename does not take. A failure here means the old contents are still in place, which is
/// the outcome to prefer: the previous state of an account store is worth more than a partial
/// new one.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());

    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension("tmp");

    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        // Before the rename, not after. This is what makes the contents survive; without it the
        // rename can reach the disk first and name an empty file.
        file.sync_all()?;
    }

    fs::rename(&temporary, path)?;

    // And the directory, because the rename is a change to the directory rather than to either
    // file. Not every filesystem needs it and none is harmed by it, so it is done everywhere
    // rather than guessed at.
    if let Some(parent) = parent {
        File::open(parent)?.sync_all()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that cleans itself up, so a failing test does not leave one behind.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("prism-store-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("makes the directory");

            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn it_writes_what_it_was_given() {
        let scratch = Scratch::new("writes");
        let path = scratch.0.join("thing.json");

        replace(&path, b"{\"a\":1}").expect("writes");

        assert_eq!(fs::read(&path).expect("reads"), b"{\"a\":1}");
    }

    #[test]
    fn it_replaces_rather_than_appends() {
        let scratch = Scratch::new("replaces");
        let path = scratch.0.join("thing.json");

        replace(&path, b"first").expect("writes");
        replace(&path, b"second").expect("writes again");

        assert_eq!(fs::read(&path).expect("reads"), b"second");
    }

    #[test]
    fn it_makes_the_directory_it_was_pointed_at() {
        let scratch = Scratch::new("makes");
        let path = scratch.0.join("nested").join("deeper").join("thing.json");

        replace(&path, b"x").expect("writes");

        assert!(path.exists());
    }

    #[test]
    fn it_leaves_no_temporary_behind() {
        let scratch = Scratch::new("tidy");
        let path = scratch.0.join("thing.json");

        replace(&path, b"x").expect("writes");

        assert!(!path.with_extension("tmp").exists());
    }
}
