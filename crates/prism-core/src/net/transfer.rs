//! Moving files between the two machines in a session.
//!
//! A remote desktop that cannot carry a file is one people work around by emailing things to
//! themselves. This is the smallest thing that removes that: one folder on each machine, and a
//! way to put something in the other one's.
//!
//! # What this module is
//!
//! A state machine and nothing else. It owns no socket, starts no thread and never blocks: a
//! caller hands it the packets that arrived and asks it what to send, and it answers one packet
//! at a time into a buffer the caller owns. That is what makes it testable without a network,
//! and what keeps the decision about *when* to send — which is a decision about whether the
//! picture stutters — outside it, with the loop that knows.
//!
//! # How a file moves
//!
//! The sender offers, the receiver answers, and then chunks flow. The receiver writes each
//! chunk straight to its place in the file rather than holding it, so one arriving out of order
//! costs a seek and no memory, and reports what it has as two numbers: the length of the
//! unbroken run from the start, and a bitmap of the thirty-two chunks after it. The sender
//! sends what those two numbers say is missing, and stops when the run reaches the end.
//!
//! # What it refuses
//!
//! A name that is not a name. Everything that arrives is written into one folder, and the wire
//! format has already refused separators and the two relative directories, so the worst a
//! hostile peer can do here is overwrite something it previously sent — which is why a file
//! lands under a temporary name and is only put in place once it is whole.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::net::packet::{
    FileAnswer, FileAsk, FileChunk, FileEntry, FileList, FileListing, FileOffer, FileRefusal,
    FileReport, FileType, MAX_FILE_PAYLOAD, MAX_PLAINTEXT_SIZE, ProtocolError, file_type_of,
    plain_file_name,
};

/// How many bytes one chunk carries.
///
/// Every chunk but the last is exactly this, which is what lets a receiver write a chunk that
/// arrived out of order straight to `index * CHUNK` without holding anything.
pub const CHUNK: usize = MAX_FILE_PAYLOAD;

/// How far ahead of what the receiver has confirmed the sender will get.
///
/// Thirty-two, because that is what one report describes: sending past the window would be
/// sending chunks the next report has no way to say arrived, and the sender would resend them
/// on the strength of its own ignorance.
const WINDOW: u32 = 32;

/// How long a sender waits for a report before sending the window again.
///
/// Reports are the only thing that moves a transfer forward, so one lost report would otherwise
/// stall the whole file until the session ended.
const REPORT_PATIENCE: Duration = Duration::from_millis(600);

/// How often the receiver says what it has.
///
/// Every eight chunks and at the end. More often would spend the return path on bookkeeping;
/// less often would leave the sender's window closed while it waited.
const REPORT_EVERY: u32 = 8;

/// The largest file this will accept, in bytes.
///
/// Two gigabytes. Not a protocol limit — the wire counts bytes in sixty-four bits — but a limit
/// on what arrives unasked over a session meant for a screen.
pub const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Why a transfer could not be done.
#[derive(Debug, Error)]
pub enum FileError {
    /// The file could not be read or written.
    #[error("{0}")]
    Io(#[from] io::Error),

    /// A packet on the file channel was not one this build could read.
    #[error("{0}")]
    Protocol(#[from] ProtocolError),

    /// The path named nothing that could be sent.
    #[error("{path} is not a file that can be sent")]
    NotAFile {
        /// What was asked for.
        path: String,
    },

    /// More bytes than this will move in one transfer.
    #[error("{size} bytes is more than one transfer carries")]
    TooLarge {
        /// How large the file is.
        size: u64,
    },

    /// A transfer was already running in that direction.
    #[error("a file is already moving that way")]
    Busy,
}

/// How far along a transfer is, for a window to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// What the file is called.
    pub name: String,
    /// How many bytes it holds.
    pub size: u64,
    /// How many of them have moved.
    pub moved: u64,
    /// Whether this machine is the one sending.
    pub sending: bool,
    /// Whether it is finished.
    pub done: bool,
}

/// A file this machine is sending.
#[derive(Debug)]
struct Sending {
    id: u32,
    name: String,
    size: u64,
    chunks: u32,
    file: File,
    /// Whether the far side has said it will take it.
    accepted: bool,
    /// How many chunks the receiver has in an unbroken run from the start.
    have: u32,
    /// Which of the thirty-two after that it also has.
    arrived: u32,
    /// The next chunk to send that has never been sent.
    fresh: u32,
    /// Chunks the newest report says are still missing.
    again: VecDeque<u32>,
    /// When the last report arrived, or when the offer went out.
    heard: Instant,
    /// Where a chunk is read to before it is encoded.
    ///
    /// One buffer for the transfer rather than one per chunk. It cannot be the caller's send
    /// buffer, because encoding writes the header over the front of that and would be writing
    /// over the bytes it is copying.
    scratch: Box<[u8]>,
}

/// A file this machine is receiving.
#[derive(Debug)]
struct Receiving {
    id: u32,
    name: String,
    size: u64,
    chunks: u32,
    file: File,
    /// Where it is being written until it is whole.
    partial: PathBuf,
    /// Where it goes when it is.
    finished: PathBuf,
    have: u32,
    arrived: u32,
    /// How many chunks have landed since the last report was sent.
    since: u32,
    /// Whether the last chunk to arrive left a gap, which is worth reporting at once.
    gap: bool,
    done: bool,
}

/// Both directions of file movement for one session.
///
/// Held by whichever loop reads the session's packets. One file may be moving each way at a
/// time; a second offer in the same direction is refused rather than queued, because a person
/// who sent two files meant to send two files and would rather be told the second did not go.
#[derive(Debug)]
pub struct Files {
    folder: PathBuf,
    sending: Option<Sending>,
    receiving: Option<Receiving>,
    /// Answers, reports and listings waiting for the caller's next buffer.
    outgoing: VecDeque<Vec<u8>>,
    /// The id the next offer from this machine will carry.
    next_id: u32,
}

impl Files {
    /// Makes the two directions for a session, dropping files into `folder`.
    ///
    /// The folder is not created here. It is made when something is first written into it, so
    /// a session where nobody moves a file leaves no trace on either machine.
    #[must_use]
    pub fn new(folder: PathBuf) -> Self {
        Self {
            folder,
            sending: None,
            receiving: None,
            outgoing: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Where files land on this machine.
    #[must_use]
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Offers a file on this machine to the other one.
    ///
    /// Nothing is read here beyond the file's length. What comes back is the transfer's id, and
    /// the file itself starts moving once the far side answers.
    ///
    /// # Errors
    ///
    /// Returns [`FileError::NotAFile`] for a path that is not a readable file with a name that
    /// could be written down at the other end, [`FileError::TooLarge`] past
    /// [`MAX_FILE_SIZE`], [`FileError::Busy`] if one is already going that way, and
    /// [`FileError::Io`] if the file will not open.
    pub fn send(&mut self, path: &Path) -> Result<u32, FileError> {
        if self.sending.is_some() {
            return Err(FileError::Busy);
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| plain_file_name(name))
            .ok_or_else(|| FileError::NotAFile {
                path: path.display().to_string(),
            })?
            .to_owned();

        let file = File::open(path)?;
        let about = file.metadata()?;

        // A directory opens on this platform and reads as nothing, so a caller that pointed at
        // one would offer a file of no bytes rather than be told it had pointed at a folder.
        if !about.is_file() {
            return Err(FileError::NotAFile {
                path: path.display().to_string(),
            });
        }

        let size = about.len();

        if size > MAX_FILE_SIZE {
            return Err(FileError::TooLarge { size });
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let chunks = chunks_for(size);
        let offer = FileOffer {
            id,
            size,
            chunks,
            name: name.clone(),
        };

        self.queue(|buf| offer.encode_into(buf), offer.encoded_len());

        self.sending = Some(Sending {
            id,
            name,
            size,
            chunks,
            file,
            accepted: false,
            have: 0,
            arrived: 0,
            fresh: 0,
            again: VecDeque::new(),
            heard: Instant::now(),
            scratch: vec![0u8; CHUNK].into_boxed_slice(),
        });

        Ok(id)
    }

    /// Asks the other machine what it is offering.
    pub fn ask_for_listing(&mut self) {
        self.queue(FileList::encode_into, 2);
    }

    /// Asks the other machine to send one of the files it listed.
    pub fn fetch(&mut self, name: &str) {
        let ask = FileAsk {
            name: name.to_owned(),
        };

        self.queue(|buf| ask.encode_into(buf), ask.encoded_len());
    }

    /// Takes one packet that arrived on the file channel.
    ///
    /// Anything it produces in reply is queued for [`Self::step`] rather than returned, so a
    /// caller reads its socket in one place and writes it in another.
    ///
    /// # Errors
    ///
    /// Returns [`FileError::Protocol`] for a packet this build cannot read, and
    /// [`FileError::Io`] if a chunk cannot be written down. Neither ends the session: a caller
    /// reports them and carries on.
    pub fn arrived(&mut self, bytes: &[u8]) -> Result<Option<Landed>, FileError> {
        match file_type_of(bytes)? {
            FileType::Offer => self.offered(&FileOffer::decode(bytes)?),
            FileType::Answer => {
                self.answered(&FileAnswer::decode(bytes)?);

                Ok(None)
            }
            FileType::Chunk => self.chunked(&FileChunk::decode(bytes)?),
            FileType::Report => {
                self.reported(&FileReport::decode(bytes)?);

                Ok(None)
            }
            FileType::List => {
                self.listed();

                Ok(None)
            }
            FileType::Listing => Ok(Some(Landed::Listing(FileListing::decode(bytes)?))),
            FileType::Ask => {
                let ask = FileAsk::decode(bytes)?;
                let path = self.folder.join(&ask.name);

                // A request for something that is not there, or for a second file while one is
                // already going, is dropped rather than answered. There is no message for
                // "no", and the side that asked knows what it asked for.
                let _ = self.send(&path);

                Ok(None)
            }
        }
    }

    /// Fills `buf` with the next packet to send, and says how many bytes it wrote.
    ///
    /// Zero means there is nothing to send this turn. A caller loops until it gets one, pacing
    /// itself between packets: this will hand over chunks as fast as it is asked, and how fast
    /// that should be is a question about the picture, which this cannot see.
    ///
    /// # Errors
    ///
    /// Returns [`FileError::Io`] if the file being sent cannot be read.
    ///
    /// # Panics
    ///
    /// Panics if `buf` is smaller than [`MAX_PLAINTEXT_SIZE`], which every caller's send buffer
    /// is at least.
    pub fn step(&mut self, buf: &mut [u8]) -> Result<usize, FileError> {
        assert!(
            buf.len() >= MAX_PLAINTEXT_SIZE,
            "a send buffer holds a full packet"
        );

        if let Some(ready) = self.outgoing.pop_front() {
            buf[..ready.len()].copy_from_slice(&ready);

            return Ok(ready.len());
        }

        self.next_chunk(buf)
    }

    /// What the transfer in each direction is doing.
    #[must_use]
    pub fn progress(&self) -> Vec<Progress> {
        let mut all = Vec::new();

        if let Some(sending) = &self.sending {
            all.push(Progress {
                name: sending.name.clone(),
                size: sending.size,
                moved: moved(sending.have, sending.size),
                sending: true,
                done: sending.have >= sending.chunks,
            });
        }

        if let Some(receiving) = &self.receiving {
            all.push(Progress {
                name: receiving.name.clone(),
                size: receiving.size,
                moved: moved(receiving.have, receiving.size),
                sending: false,
                done: receiving.done,
            });
        }

        all
    }

    /// Reads what this machine is offering, newest first.
    ///
    /// A folder that does not exist yet lists as empty, which is what it holds.
    #[must_use]
    pub fn listing(&self) -> FileListing {
        let mut found: Vec<(std::time::SystemTime, FileEntry)> = Vec::new();

        let Ok(entries) = fs::read_dir(&self.folder) else {
            return FileListing {
                more: false,
                files: Vec::new(),
            };
        };

        for entry in entries.flatten() {
            let Ok(data) = entry.metadata() else {
                continue;
            };

            if !data.is_file() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };

            // Partly-arrived files are not offered. Something still being written is not a
            // thing anybody meant to hand over.
            if !plain_file_name(&name) || name.ends_with(PARTIAL) {
                continue;
            }

            found.push((
                data.modified().unwrap_or(std::time::UNIX_EPOCH),
                FileEntry {
                    size: data.len(),
                    name,
                },
            ));
        }

        found.sort_by_key(|(when, _)| std::cmp::Reverse(*when));

        let mut files = Vec::new();
        let mut room = MAX_PLAINTEXT_SIZE - 5;
        let mut more = false;

        for (_, entry) in found {
            let cost = 9 + entry.name.len();

            if cost > room {
                more = true;
                break;
            }

            room -= cost;
            files.push(entry);
        }

        FileListing { more, files }
    }

    /// Answers an offer from the far machine, taking it unless there is a reason not to.
    fn offered(&mut self, offer: &FileOffer) -> Result<Option<Landed>, FileError> {
        let refuse = |files: &mut Self, refusal: FileRefusal| {
            let answer = FileAnswer {
                id: offer.id,
                accepted: false,
                refusal,
            };

            files.queue(|buf| answer.encode_into(buf), 8);
        };

        if self.receiving.is_some() {
            refuse(self, FileRefusal::Declined);

            return Ok(None);
        }

        if offer.size > MAX_FILE_SIZE || offer.chunks != chunks_for(offer.size) {
            refuse(self, FileRefusal::TooLarge);

            return Ok(None);
        }

        let partial = self.folder.join(format!("{}{PARTIAL}", offer.name));
        let finished = self.folder.join(&offer.name);

        if fs::create_dir_all(&self.folder).is_err() {
            refuse(self, FileRefusal::NotWritable);

            return Ok(None);
        }

        let Ok(file) = File::create(&partial) else {
            refuse(self, FileRefusal::NotWritable);

            return Ok(None);
        };

        let answer = FileAnswer {
            id: offer.id,
            accepted: true,
            refusal: FileRefusal::Declined,
        };

        self.queue(|buf| answer.encode_into(buf), 8);

        let done = offer.chunks == 0;

        self.receiving = Some(Receiving {
            id: offer.id,
            name: offer.name.clone(),
            size: offer.size,
            chunks: offer.chunks,
            file,
            partial,
            finished,
            have: 0,
            arrived: 0,
            since: 0,
            gap: false,
            done: false,
        });

        // A file of nothing is whole the moment it is accepted, and never sends a chunk to
        // notice that by.
        if done {
            return self.settle();
        }

        Ok(None)
    }

    /// Records the far machine's answer to what this one offered.
    fn answered(&mut self, answer: &FileAnswer) {
        let Some(sending) = &mut self.sending else {
            return;
        };

        if sending.id != answer.id {
            return;
        }

        if answer.accepted {
            sending.accepted = true;
            sending.heard = Instant::now();
        } else {
            self.sending = None;
        }
    }

    /// Writes a chunk into the file it belongs to.
    fn chunked(&mut self, chunk: &FileChunk<'_>) -> Result<Option<Landed>, FileError> {
        let Some(receiving) = &mut self.receiving else {
            return Ok(None);
        };

        if receiving.id != chunk.id || chunk.index >= receiving.chunks || receiving.done {
            return Ok(None);
        }

        // Already settled, and rewriting it would be taking a resend at its word about bytes
        // that are already on disk.
        if chunk.index < receiving.have {
            return Ok(None);
        }

        let offset = u64::from(chunk.index) * CHUNK as u64;
        receiving.file.seek(SeekFrom::Start(offset))?;
        receiving.file.write_all(chunk.payload)?;

        let step = chunk.index - receiving.have;

        if step == 0 {
            receiving.have += 1;

            // Everything the bitmap already knew about now sits at the front of the run, so
            // walk it forward and shift what is left.
            while receiving.arrived & 1 != 0 {
                receiving.have += 1;
                receiving.arrived >>= 1;
            }

            receiving.arrived >>= 1;
        } else if step < 32 {
            receiving.arrived |= 1 << (step - 1);
            receiving.gap = true;
        }

        receiving.since += 1;

        let whole = receiving.have >= receiving.chunks;

        if whole || receiving.gap || receiving.since >= REPORT_EVERY {
            let report = FileReport {
                id: receiving.id,
                have: receiving.have,
                arrived: receiving.arrived,
            };

            receiving.since = 0;
            receiving.gap = false;
            self.queue(|buf| report.encode_into(buf), 14);
        }

        if whole {
            return self.settle();
        }

        Ok(None)
    }

    /// Puts a finished file in its place and says what landed.
    fn settle(&mut self) -> Result<Option<Landed>, FileError> {
        let Some(receiving) = &mut self.receiving else {
            return Ok(None);
        };

        receiving.file.flush()?;
        receiving.file.sync_all()?;
        fs::rename(&receiving.partial, &receiving.finished)?;
        receiving.done = true;

        Ok(Some(Landed::Received {
            name: receiving.name.clone(),
            path: receiving.finished.clone(),
        }))
    }

    /// Records what the receiver says it has, and lines up whatever is missing.
    fn reported(&mut self, report: &FileReport) {
        let Some(sending) = &mut self.sending else {
            return;
        };

        if sending.id != report.id || report.have < sending.have {
            return;
        }

        sending.have = report.have;
        sending.arrived = report.arrived;
        sending.heard = Instant::now();
        sending.again.clear();

        if sending.have >= sending.chunks {
            self.sending = None;

            return;
        }

        // Everything inside the window whose bit is clear and which has already been sent
        // once. The first is the one the run stopped at, which is missing by definition.
        sending.again.push_back(sending.have);

        for step in 1..WINDOW {
            let index = sending.have + step;

            if index >= sending.fresh || index >= sending.chunks {
                break;
            }

            if sending.arrived & (1 << (step - 1)) == 0 {
                sending.again.push_back(index);
            }
        }
    }

    /// Answers a request for what this machine is offering.
    fn listed(&mut self) {
        let listing = self.listing();

        self.queue(|buf| listing.encode_into(buf), listing.encoded_len());
    }

    /// Fills `buf` with the next chunk of the file being sent, if one is due.
    fn next_chunk(&mut self, buf: &mut [u8]) -> Result<usize, FileError> {
        let Some(sending) = &mut self.sending else {
            return Ok(0);
        };

        if !sending.accepted {
            return Ok(0);
        }

        // A report that never came. Sending the window again is what gets a transfer past a
        // lost report, which would otherwise stop it for good.
        if sending.again.is_empty()
            && sending.fresh >= (sending.have + WINDOW).min(sending.chunks)
            && sending.heard.elapsed() >= REPORT_PATIENCE
        {
            sending.fresh = sending.have;
            sending.heard = Instant::now();
        }

        let index = match sending.again.pop_front() {
            Some(index) => index,
            None if sending.fresh < (sending.have + WINDOW).min(sending.chunks) => {
                let index = sending.fresh;
                sending.fresh += 1;

                index
            }
            None => return Ok(0),
        };

        let offset = u64::from(index) * CHUNK as u64;
        let length = (sending.size - offset).min(CHUNK as u64) as usize;

        sending.file.seek(SeekFrom::Start(offset))?;
        sending.file.read_exact(&mut sending.scratch[..length])?;

        let chunk = FileChunk {
            id: sending.id,
            index,
            payload: &sending.scratch[..length],
        };

        Ok(chunk.encode_into(buf)?)
    }

    /// Puts a message on the queue for the caller's next turn.
    ///
    /// The closure writes into a buffer sized by `needed`; a message that will not encode is
    /// dropped, because every one of them is built from fields this module chose.
    fn queue<F>(&mut self, write: F, needed: usize)
    where
        F: FnOnce(&mut [u8]) -> Result<usize, ProtocolError>,
    {
        let mut bytes = vec![0u8; needed];

        if let Ok(written) = write(&mut bytes) {
            bytes.truncate(written);
            self.outgoing.push_back(bytes);
        }
    }
}

/// The folder the two machines hand things to each other through.
///
/// One place with a name somebody can find, rather than the downloads folder: what arrives
/// here arrived because another machine sent it, and mixing that in with what a browser
/// downloaded would make it impossible to tell the two apart afterwards.
///
/// Returns `None` on a machine with no home directory, where there is nowhere to put it.
#[must_use]
pub fn shared_folder() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())?;

    Some(PathBuf::from(home).join("Prism"))
}

/// Something the far machine sent that the caller has to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    /// A file arrived whole.
    Received {
        /// What it is called.
        name: String,
        /// Where it was put.
        path: PathBuf,
    },
    /// The far machine said what it is offering.
    Listing(FileListing),
}

/// What a partly arrived file is called while it is arriving.
const PARTIAL: &str = ".prism-part";

/// How many chunks a file of this many bytes is cut into.
fn chunks_for(size: u64) -> u32 {
    size.div_ceil(CHUNK as u64) as u32
}

/// How many bytes an unbroken run of chunks accounts for, capped at the file's length.
fn moved(have: u32, size: u64) -> u64 {
    (u64::from(have) * CHUNK as u64).min(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs both ends until neither has anything more to send.
    ///
    /// Every packet one produces is handed to the other, which is what a session does with no
    /// loss in it. Returns what landed on each side.
    fn settle(left: &mut Files, right: &mut Files) -> (Vec<Landed>, Vec<Landed>) {
        let mut buf = [0u8; MAX_PLAINTEXT_SIZE];
        let (mut on_left, mut on_right) = (Vec::new(), Vec::new());

        for _ in 0..10_000 {
            let from_left = left.step(&mut buf).expect("left steps");

            if from_left > 0 {
                if let Some(landed) = right.arrived(&buf[..from_left]).expect("right takes it") {
                    on_right.push(landed);
                }

                continue;
            }

            let from_right = right.step(&mut buf).expect("right steps");

            if from_right > 0 {
                if let Some(landed) = left.arrived(&buf[..from_right]).expect("left takes it") {
                    on_left.push(landed);
                }

                continue;
            }

            break;
        }

        (on_left, on_right)
    }

    /// Makes a file of `size` bytes whose contents depend on where they are.
    fn written(at: &Path, name: &str, size: usize) -> PathBuf {
        let path = at.join(name);
        let body: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        fs::create_dir_all(at).expect("the folder is made");
        fs::write(&path, &body).expect("the file is written");

        path
    }

    /// Two machines with a folder each, under a directory of this test's own.
    fn pair(name: &str) -> (PathBuf, PathBuf, Files, Files) {
        let root = std::env::temp_dir().join(format!("prism-transfer-{name}"));
        let _ = fs::remove_dir_all(&root);

        let here = root.join("here");
        let there = root.join("there");

        (
            here.clone(),
            there.clone(),
            Files::new(here),
            Files::new(there),
        )
    }

    #[test]
    fn a_file_arrives_whole() {
        let (here, there, mut left, mut right) = pair("whole");
        let source = written(&here.join("out"), "notes.txt", CHUNK * 3 + 17);

        left.send(&source).expect("the offer is made");

        let (_, on_right) = settle(&mut left, &mut right);

        assert_eq!(
            on_right,
            vec![Landed::Received {
                name: "notes.txt".to_owned(),
                path: there.join("notes.txt"),
            }]
        );
        assert_eq!(
            fs::read(there.join("notes.txt")).expect("it is there"),
            fs::read(&source).expect("it is still here"),
        );
    }

    #[test]
    fn a_file_of_nothing_arrives() {
        let (here, there, mut left, mut right) = pair("empty");
        let source = written(&here.join("out"), "empty.bin", 0);

        left.send(&source).expect("the offer is made");
        settle(&mut left, &mut right);

        assert!(there.join("empty.bin").exists());
        assert_eq!(fs::metadata(there.join("empty.bin")).unwrap().len(), 0);
    }

    #[test]
    fn what_the_wire_drops_is_sent_again() {
        let (here, there, mut left, mut right) = pair("lossy");
        let source = written(&here.join("out"), "big.bin", CHUNK * 40 + 5);

        left.send(&source).expect("the offer is made");

        let mut buf = [0u8; MAX_PLAINTEXT_SIZE];
        let mut seen = 0u32;

        for _ in 0..100_000 {
            let from_left = left.step(&mut buf).expect("left steps");

            if from_left > 0 {
                seen += 1;

                // Every seventh packet never arrives, which is a worse wire than any real one
                // and is the point: the repair has to work without help.
                if seen % 7 != 0 {
                    right.arrived(&buf[..from_left]).expect("right takes it");
                }

                continue;
            }

            let from_right = right.step(&mut buf).expect("right steps");

            if from_right > 0 {
                left.arrived(&buf[..from_right]).expect("left takes it");

                continue;
            }

            break;
        }

        assert_eq!(
            fs::read(there.join("big.bin")).expect("it is there"),
            fs::read(&source).expect("it is still here"),
        );
    }

    #[test]
    fn a_listing_says_what_is_there_and_a_request_sends_it() {
        let (here, there, mut left, mut right) = pair("listing");
        written(&here, "one.txt", 10);
        written(&here, "two.txt", CHUNK + 1);

        right.ask_for_listing();

        let (on_right, _) = settle(&mut right, &mut left);
        let Some(Landed::Listing(listing)) = on_right.first() else {
            panic!("the listing should have arrived: {on_right:?}");
        };

        let mut names: Vec<&str> = listing.files.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["one.txt", "two.txt"]);

        right.fetch("two.txt");
        settle(&mut right, &mut left);

        assert_eq!(
            fs::read(there.join("two.txt")).expect("it was fetched"),
            fs::read(here.join("two.txt")).expect("it is still there"),
        );
    }

    #[test]
    fn a_second_file_in_the_same_direction_is_refused() {
        let (here, _, mut left, _) = pair("busy");
        let source = written(&here.join("out"), "a.bin", 3);

        left.send(&source).expect("the first goes");
        assert!(matches!(left.send(&source), Err(FileError::Busy)));
    }

    #[test]
    fn a_name_that_is_not_a_name_is_not_sent() {
        let (here, _, mut left, _) = pair("named");
        let folder = here.join("out");
        fs::create_dir_all(&folder).expect("the folder is made");

        assert!(matches!(
            left.send(&folder),
            Err(FileError::NotAFile { .. })
        ));
    }

    #[test]
    fn nothing_is_left_in_place_until_it_is_whole() {
        let (here, there, mut left, mut right) = pair("partial");
        let source = written(&here.join("out"), "slow.bin", CHUNK * 4);

        left.send(&source).expect("the offer is made");

        let mut buf = [0u8; MAX_PLAINTEXT_SIZE];
        let mut pass = |from: &mut Files, to: &mut Files| {
            let written = from.step(&mut buf).expect("it steps");
            assert!(written > 0, "there should be something to send");
            to.arrived(&buf[..written]).expect("it takes it");
        };

        // The offer, the answer, and then far enough to have written something and not far
        // enough to have written all of it.
        pass(&mut left, &mut right);
        pass(&mut right, &mut left);
        pass(&mut left, &mut right);
        pass(&mut left, &mut right);

        assert!(!there.join("slow.bin").exists());
        assert!(there.join(format!("slow.bin{PARTIAL}")).exists());
    }
}
