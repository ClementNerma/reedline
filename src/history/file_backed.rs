use indexmap::IndexMap;
use rand::{rngs::SmallRng, Rng, SeedableRng};

use super::{
    base::CommandLineSearch, History, HistoryItem, HistoryItemId, SearchDirection, SearchQuery,
};
use crate::{
    result::{ReedlineError, ReedlineErrorVariants},
    HistorySessionId, Result,
};

use std::{
    fs::OpenOptions,
    hash::{DefaultHasher, Hash, Hasher},
    io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write},
    ops::{Deref, DerefMut},
    path::PathBuf,
};

/// Default size of the [`FileBackedHistory`] used when calling [`FileBackedHistory::default()`]
pub const HISTORY_SIZE: usize = 1000;
pub const NEWLINE_ESCAPE: &str = "<\\n>";

/// Stateful history that allows up/down-arrow browsing with an internal cursor.
///
/// Can optionally be associated with a newline separated history file using the [`FileBackedHistory::with_file()`] constructor.
/// Similar to bash's behavior without HISTTIMEFORMAT.
/// (See <https://www.gnu.org/software/bash/manual/html_node/Bash-History-Facilities.html>)
/// If the history is associated to a file all new changes within a given history capacity will be written to disk when History is dropped.
#[derive(Debug)]
pub struct FileBackedHistory {
    capacity: usize,
    entries: IndexMap<HistoryItemId, String>,
    file: Option<PathBuf>,
    last_on_disk: Option<HistoryItemId>,
    session: Option<HistorySessionId>,
    rng: SmallRng,
}

impl Default for FileBackedHistory {
    /// Creates an in-memory [`History`] with a maximal capacity of [`HISTORY_SIZE`].
    ///
    /// To create a [`History`] that is synchronized with a file use [`FileBackedHistory::with_file()`]
    ///
    /// # Panics
    ///
    /// If `HISTORY_SIZE == usize::MAX`
    fn default() -> Self {
        match Self::new(HISTORY_SIZE) {
            Ok(history) => history,
            Err(e) => panic!("{}", e),
        }
    }
}

fn encode_entry(s: &str) -> String {
    s.replace('\n', NEWLINE_ESCAPE)
}

/// Decode an entry
///
/// Legacy format: ls /
/// New format   : 182535<id>:ls /
///
/// If a line can't be parsed using the new format, it will fallback to the legacy one.
///
/// This allows this function to support decoding for both legacy and new histories,
/// as well as mixing both of them.
fn decode_entry(s: &str, counter: &mut i64) -> (HistoryItemId, String) {
    let mut hasher = DefaultHasher::new();
    counter.hash(&mut hasher);
    s.hash(&mut hasher);

    let id = hasher.finish() as i64;

    (HistoryItemId(id), s.replace(NEWLINE_ESCAPE, "\n"))
}

impl History for FileBackedHistory {
    fn generate_id(&mut self) -> HistoryItemId {
        HistoryItemId(self.rng.gen())
    }

    /// only saves a value if it's different than the last value
    fn save(&mut self, h: &HistoryItem) -> Result<()> {
        let entry = h.command_line.clone();

        // Don't append if the preceding value is identical or the string empty
        if self
            .entries
            .last()
            .map_or(true, |(_, previous)| previous != &entry)
            && !entry.is_empty()
            && self.capacity > 0
        {
            if self.entries.len() >= self.capacity {
                // History is "full", so we delete the oldest entry first,
                // before adding a new one.
                let first_id = *(self.entries.first().unwrap().0);
                let prev = self.entries.shift_remove(&first_id);
                assert!(prev.is_some());
            }

            self.entries.insert(h.id, entry.to_string());
        }

        Ok(())
    }

    /// this history doesn't replace entries
    fn replace(&mut self, h: &HistoryItem) -> Result<()> {
        self.save(h)
    }

    fn load(&self, id: HistoryItemId) -> Result<HistoryItem> {
        println!("{:?}", self.entries);

        Ok(FileBackedHistory::construct_entry(
            id,
            self.entries
                .get(&id)
                .ok_or(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                    "Item does not exist",
                )))?
                .clone(),
        ))
    }

    fn count(&self, query: SearchQuery) -> Result<u64> {
        // todo: this could be done cheaper
        Ok(self.search(query)?.len() as u64)
    }

    fn search(&self, query: SearchQuery) -> Result<Vec<HistoryItem>> {
        // Destructure the query - this ensures that if another element is added to this type later on,
        // we won't forget to update this function as the destructuring will then be incomplete.
        let SearchQuery {
            direction,
            start_time,
            end_time,
            start_id,
            end_id,
            limit,
            filter,
        } = query;

        if start_time.is_some() || end_time.is_some() {
            return Err(ReedlineError(
                ReedlineErrorVariants::HistoryFeatureUnsupported {
                    history: "FileBackedHistory",
                    feature: "filtering by time",
                },
            ));
        }

        if filter.hostname.is_some()
            || filter.cwd_exact.is_some()
            || filter.cwd_prefix.is_some()
            || filter.exit_successful.is_some()
        {
            return Err(ReedlineError(
                ReedlineErrorVariants::HistoryFeatureUnsupported {
                    history: "FileBackedHistory",
                    feature: "filtering by extra info",
                },
            ));
        }

        let (start_id, end_id) = {
            if let SearchDirection::Backward = direction {
                (end_id, start_id)
            } else {
                (start_id, end_id)
            }
        };

        let start_idx = match start_id {
            Some(from_id) => self.entries.get_index_of(&from_id).ok_or(ReedlineError(
                ReedlineErrorVariants::OtherHistoryError("provided 'start_id' item was not found"),
            ))?,
            None => 0,
        };

        let end_idx = match end_id {
            Some(to_id) => self.entries.get_index_of(&to_id).ok_or(ReedlineError(
                ReedlineErrorVariants::OtherHistoryError("provided 'end_id' item was not found"),
            ))?,
            None => self.entries.len().saturating_sub(1),
        };

        assert!(start_idx <= end_idx);

        let iter = self
            .entries
            .iter()
            .skip(start_idx)
            .take(1 + end_idx - start_idx);

        let limit = limit
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(usize::MAX);

        let filter = |(id, cmd): (&HistoryItemId, &String)| {
            let str_matches = match &filter.command_line {
                Some(CommandLineSearch::Prefix(p)) => cmd.starts_with(p),
                Some(CommandLineSearch::Substring(p)) => cmd.contains(p),
                Some(CommandLineSearch::Exact(p)) => cmd == p,
                None => true,
            };

            if !str_matches {
                return None;
            }

            if let Some(str) = &filter.not_command_line {
                if cmd == str {
                    return None;
                }
            }

            Some(FileBackedHistory::construct_entry(
                *id,
                cmd.clone(), // todo: this cloning might be a perf bottleneck
            ))
        };

        Ok(match query.direction {
            SearchDirection::Backward => iter.rev().filter_map(filter).take(limit).collect(),
            SearchDirection::Forward => iter.filter_map(filter).take(limit).collect(),
        })
    }

    fn update(
        &mut self,
        _id: super::HistoryItemId,
        _updater: &dyn Fn(super::HistoryItem) -> super::HistoryItem,
    ) -> Result<()> {
        Err(ReedlineError(
            ReedlineErrorVariants::HistoryFeatureUnsupported {
                history: "FileBackedHistory",
                feature: "updating entries",
            },
        ))
    }

    fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        self.last_on_disk = None;

        if let Some(file) = &self.file {
            if let Err(err) = std::fs::remove_file(file) {
                return Err(ReedlineError(ReedlineErrorVariants::IOError(err)));
            }
        }

        Ok(())
    }

    fn delete(&mut self, _h: super::HistoryItemId) -> Result<()> {
        Err(ReedlineError(
            ReedlineErrorVariants::HistoryFeatureUnsupported {
                history: "FileBackedHistory",
                feature: "removing entries",
            },
        ))
    }

    /// Writes unwritten history contents to disk.
    ///
    /// If file would exceed `capacity` truncates the oldest entries.
    fn sync(&mut self) -> std::io::Result<()> {
        let Some(fname) = &self.file else {
            return Ok(());
        };

        // The unwritten entries
        let last_index_on_disk = self
            .last_on_disk
            .map(|id| self.entries.get_index_of(&id).unwrap());

        let range_start = match last_index_on_disk {
            Some(index) => index + 1,
            None => 0,
        };

        let own_entries = self.entries.get_range(range_start..).unwrap();

        if let Some(base_dir) = fname.parent() {
            std::fs::create_dir_all(base_dir)?;
        }

        let mut f_lock = fd_lock::RwLock::new(
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .read(true)
                .open(fname)?,
        );

        let mut writer_guard = f_lock.write()?;

        let (mut foreign_entries, truncate) = {
            let reader = BufReader::new(writer_guard.deref());

            let mut counter = 0;

            let mut from_file = reader
                .lines()
                .map(|o| o.map(|i| decode_entry(&i, &mut counter)))
                .collect::<std::io::Result<IndexMap<_, _>>>()?;

            if from_file.len() + own_entries.len() > self.capacity {
                let start = from_file.len() + own_entries.len() - self.capacity;

                (from_file.split_off(start), true)
            } else {
                (from_file, false)
            }
        };

        {
            let mut writer = BufWriter::new(writer_guard.deref_mut());

            // In case of truncation, we first write every foreign entry (replacing existing content)
            if truncate {
                writer.rewind()?;

                for line in foreign_entries.values() {
                    writer.write_all(encode_entry(line).as_bytes())?;
                    writer.write_all("\n".as_bytes())?;
                }
            } else {
                // Otherwise we directly jump at the end of the file
                writer.seek(SeekFrom::End(0))?;
            }

            // Then we write new entries (that haven't been synced to the file yet)
            for line in own_entries.values() {
                writer.write_all(encode_entry(line).as_bytes())?;
                writer.write_all("\n".as_bytes())?;
            }

            writer.flush()?;
        }

        // If truncation is needed, we then remove everything after the cursor's current location
        if truncate {
            let file = writer_guard.deref_mut();
            let file_len = file.stream_position()?;
            file.set_len(file_len)?;
        }

        match last_index_on_disk {
            Some(last_index_on_disk) => {
                if last_index_on_disk + 1 < self.entries.len() {
                    foreign_entries.extend(self.entries.drain(last_index_on_disk + 1..));
                }
            }

            None => {
                foreign_entries.extend(self.entries.drain(..));
            }
        }

        self.entries = foreign_entries;

        self.last_on_disk = self.entries.last().map(|(id, _)| *id);

        Ok(())
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.session
    }
}

impl FileBackedHistory {
    /// Creates a new in-memory history that remembers `n <= capacity` elements
    ///
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == usize::MAX {
            return Err(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                "History capacity too large to be addressed safely",
            )));
        }

        Ok(FileBackedHistory {
            capacity,
            entries: IndexMap::new(),
            file: None,
            last_on_disk: None,
            session: None,
            rng: SmallRng::from_entropy(),
        })
    }

    /// Creates a new history with an associated history file.
    ///
    /// History file format: commands separated by new lines.
    /// If file exists file will be read otherwise empty file will be created.
    ///
    ///
    /// **Side effects:** creates all nested directories to the file
    ///
    pub fn with_file(capacity: usize, file: PathBuf) -> Result<Self> {
        let mut hist = Self::new(capacity)?;

        if let Some(base_dir) = file.parent() {
            std::fs::create_dir_all(base_dir)
                .map_err(ReedlineErrorVariants::IOError)
                .map_err(ReedlineError)?;
        }

        hist.file = Some(file);
        hist.sync()?;

        Ok(hist)
    }

    // this history doesn't store any info except command line
    fn construct_entry(id: HistoryItemId, command_line: String) -> HistoryItem {
        HistoryItem {
            id,
            start_timestamp: None,
            command_line,
            session_id: None,
            hostname: None,
            cwd: None,
            duration: None,
            exit_status: None,
            more_info: None,
        }
    }
}

impl Drop for FileBackedHistory {
    /// On drop the content of the [`History`] will be written to the file if specified via [`FileBackedHistory::with_file()`].
    fn drop(&mut self) {
        let _res = self.sync();
    }
}
