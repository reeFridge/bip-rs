use std::io;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::path::{PathBuf, Path};
use std::cmp;
use std::cell::RefCell;

use bip_metainfo::{MetainfoFile, InfoDictionary, File};
use bip_util::bt::InfoHash;
use bip_util::send::TrySender;
use chan::{self, Sender, Receiver};

use disk::worker::shared::blocks::Blocks;
use disk::worker::shared::clients::Clients;
use disk::error::{TorrentResult, TorrentError, TorrentErrorKind};
use disk::worker::disk_worker::piece_reader::PieceReader;
use disk::{IDiskMessage, ODiskMessage};
use disk::worker::{self, ReserveBlockClientMetadata, SyncBlockMessage, AsyncBlockMessage, DiskMessage};
use disk::worker::disk_worker::context::DiskWorkerContext;
use disk::fs::{FileSystem};
use token::{Token, TokenGenerator};
use message::standard::PieceMessage;

/// Calculates hashes on existing files within the file system given and reports good/bad pieces.
pub struct PieceChecker<'a, F> {
    fs:            F,
    info_dict:     &'a InfoDictionary,
    checker_state: PieceCheckerState
}

impl<'a, F> PieceChecker<'a, F> where F: FileSystem + 'a {
    /// Create a new PieceChecker with an initialized state.
    pub fn new(fs: F, info_dict: &'a InfoDictionary) -> TorrentResult<PieceChecker<'a, F>> {
        let mut piece_checker = PieceChecker::with_state(fs, info_dict, PieceCheckerState::new());
        
        try!(piece_checker.validate_files_sizes());
        try!(piece_checker.fill_checker_state());
        
        Ok(piece_checker)
    }

    /// Create a new PieceChecker with the given state.
    pub fn with_state(fs: F, info_dict: &'a InfoDictionary, checker_state: PieceCheckerState) -> PieceChecker<'a, F> {
        PieceChecker {
            fs:            fs,
            info_dict:     info_dict,
            checker_state: checker_state
        }
    }

    /// Calculate the diff of old to new good/bad pieces and store them in the piece checker state
    /// to be retrieved by the caller.
    pub fn calculate_diff(mut self) -> TorrentResult<PieceCheckerState> {
        let piece_length = self.info_dict.piece_length() as u64;
        // TODO: Use Block Allocator
        let mut piece_buffer = vec![0u8; piece_length as usize];

        let info_dict = self.info_dict;
        let piece_reader = PieceReader::new(self.fs, self.info_dict);

        try!(self.checker_state.run_with_whole_pieces(piece_length as usize, |message| {
            try!(piece_reader.read_piece(&mut piece_buffer[..], message));
            
            let calculated_hash = InfoHash::from_bytes(&piece_buffer);
            let expected_hash = InfoHash::from_hash(info_dict
                .pieces()
                .skip(message.piece_index() as usize)
                .next()
                .expect("bip_peer: Piece Checker Failed To Retrieve Expected Hash"))
                .expect("bip_peer: Wrong Length Of Expected Hash Received");

            Ok(calculated_hash == expected_hash)
        }));

        Ok(self.checker_state)
    }

    /// Fill the PieceCheckerState with all piece messages for each file in our info dictionary.
    ///
    /// This is done once when a torrent file is added to see if we have any good pieces that
    /// the caller can use to skip (if the torrent was partially downloaded before).
    fn fill_checker_state(&mut self) -> TorrentResult<()> {
        for piece_index in 0..self.info_dict.pieces().count() {
            self.checker_state.add_pending_block(PieceMessage::new(piece_index as u32, 0, self.info_dict.piece_length() as usize));
        }

        Ok(())
    }

    /// Validates the file sizes for the given torrent file and block allocates them if they do not exist.
    ///
    /// This function will, if the file does not exist, or exists and is zero size, fill the file with zeroes.
    /// Otherwise, if the file exists and it is of the correct size, it will be left alone. If it is of the wrong
    /// size, an error will be thrown as we do not want to overwrite and existing file that maybe just had the same
    /// name as a file in our dictionary.
    fn validate_files_sizes(&mut self) -> TorrentResult<()> {
        for file in self.info_dict.files() {
            let file_path = build_path(self.info_dict.directory(), file);
            let expected_size = file.length() as u64;

            try!(self.fs.open_file(Some(&file_path))
                .map_err(|err| err.into())
                .and_then(|mut file| {
                // File May Or May Not Have Existed Before, If The File Is Zero
                // Length, Assume It Wasn't There (User Doesn't Lose Any Data)
                let actual_size = try!(self.fs.file_size(&file));

                let size_matches = actual_size == expected_size;
                let size_is_zero = actual_size == 0;

                if !size_matches && size_is_zero {
                    self.fs.write_file(&mut file, expected_size - 1, &[0]);
                } else if !size_matches {
                    return Err(TorrentError::from_kind(TorrentErrorKind::ExistingFileSizeCheck{
                        file_path: file_path,
                        expected_size: expected_size,
                        actual_size: actual_size
                    }))
                }
                
                Ok(())
            }));
        }

        Ok(())
    }
}

fn build_path(parent_directory: Option<&str>, file: &File) -> String {
    let parent_directory = parent_directory.unwrap_or(".");

    file.paths().fold(parent_directory.to_string(), |mut acc, item| {
        acc.push_str("/");
        acc.push_str(item);

        acc
    })
}

// ----------------------------------------------------------------------------//

/// Stores state for the PieceChecker between invocations.
pub struct PieceCheckerState {
    new_states:     Vec<PieceState>,
    old_states:     HashSet<PieceState>,
    pending_blocks: HashMap<u32, Vec<PieceMessage>>
}

#[derive(PartialEq, Eq, Hash)]
pub enum PieceState {
    /// Piece was discovered as good.
    Good(u32),
    /// Piece was discovered as bad.
    Bad(u32)
}

impl PieceCheckerState {
    /// Create a new PieceCheckerState.
    fn new() -> PieceCheckerState {
        PieceCheckerState {
            new_states: Vec::new(),
            old_states: HashSet::new(),
            pending_blocks: HashMap::new()
        }
    }

    /// Add a pending piece block to the current pending blocks.
    pub fn add_pending_block(&mut self, msg: PieceMessage) {
        self.pending_blocks.entry(msg.piece_index()).or_insert(Vec::new()).push(msg);
    }
    
    /// Run the given closures against NewGood and NewBad messages. Each of the messages will
    /// then either be dropped (NewBad) or converted to OldGood (NewGood).
    pub fn run_with_diff<F>(&mut self, mut callback: F)
        where F: FnMut(&PieceState) {
        for piece_state in self.new_states.drain(..) {
            callback(&piece_state);

            self.old_states.insert(piece_state);
        }
    }

    /// Pass any pieces that have not been identified as OldGood into the callback which determines
    /// if the piece is good or bad so it can be marked as NewGood or NewBad.
    fn run_with_whole_pieces<F>(&mut self, piece_length: usize, mut callback: F) -> TorrentResult<()>
        where F: FnMut(&PieceMessage) -> TorrentResult<bool> {
        self.merge_pieces();

        let mut new_states = &mut self.new_states;
        let old_states = &self.old_states;

        for ref message in self.pending_blocks.values()
            .filter(|ref messages| messages.len() == 1 && messages[0].block_length() == piece_length)
            .map(|ref messages| messages[0])
            .filter(|ref message| !old_states.contains(&PieceState::Good(message.piece_index()))) {
            let is_good = try!(callback(message));

            if is_good {
                new_states.push(PieceState::Good(message.piece_index()));
            } else {
                new_states.push(PieceState::Bad(message.piece_index()));
            }
        }

        Ok(())
    }

    /// Merges all pending piece messages into a single messages if possible.
    fn merge_pieces(&mut self) {
        for (ref index, ref mut messages) in self.pending_blocks.iter_mut() {
            // Sort the messages by their block offset
            messages.sort_by(|a, b| a.block_offset().cmp(&b.block_offset()));

            let mut messages_len = messages.len();
            let mut merge_success = true;
            // See if we can merge all messages into a single message
            while merge_success && messages_len > 1 {
                let actual_last = messages.remove(messages_len - 1);
                let second_last = messages.remove(messages_len - 1);

                let opt_merged =  merge_piece_messages(&actual_last, &second_last);
                if let Some(merged) = opt_merged {
                    messages.push(merged);
                } else {
                    messages.push(second_last);
                    messages.push(actual_last);

                    merge_success = false;
                }

                messages_len = messages.len();
            }
        }
    }
}

/// Merge a piece message a with a piece message b if possible.
fn merge_piece_messages(message_a: &PieceMessage, message_b: &PieceMessage) -> Option<PieceMessage> {
    let start_a = message_a.block_offset();
    let end_a = start_a + message_a.block_length() as u32;

    let start_b = message_b.block_offset();
    let end_b = start_b + message_b.block_length() as u32;

    let max_end = cmp::max(end_a, end_b);

    // Check which start to use, assuming the start falls between the other messages start to end range
    if start_b >= start_a && start_b <= end_a {
        Some(PieceMessage::new(message_a.piece_index(), start_a, max_end as usize))
    } else if start_a >= start_b && start_a <= end_b {
        Some(PieceMessage::new(message_a.piece_index(), start_b, max_end as usize))
    } else {
        None
    }
}