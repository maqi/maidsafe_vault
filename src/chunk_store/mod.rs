// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

//! # Chunk Store
//! A simple, non-persistent, disk-based key-value store.

use fs2::FileExt;
use hex::ToHex;
use maidsafe_utilities::serialisation::{self, SerialisationError};
use serde::{Deserialize, Serialize};
use std::cmp;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use maidsafe_utilities::thread;
use rust_sodium::crypto::hash::sha256;
use std::collections::HashMap;
use std::str::FromStr;
use std::string::String;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The max name length for a chunk file.
const MAX_CHUNK_FILE_NAME_LENGTH: usize = 104;
/// The name of the lock file for the chunk directory.
const LOCK_FILE_NAME: &'static str = "lock";

quick_error! {
    /// `ChunkStore` error.
    #[derive(Debug)]
    pub enum Error {
        /// Error during filesystem IO operations.
        Io(error: io::Error) {
            description("IO error")
            display("IO error: {}", error)
            cause(error)
            from()
        }
        /// Error during serialisation or deserialisation of keys or values.
        Serialisation(error: SerialisationError) {
            description("Serialisation error")
            display("Serialisation error: {}", error)
            cause(error)
            from()
        }
        /// Not enough space in `ChunkStore` to perform `put`.
        NotEnoughSpace {
            description("Not enough space")
            display("Not enough space")
        }
        /// Key, Value pair not found in `ChunkStore`.
        NotFound {
            description("Key, Value not found")
            display("Key, Value not found")
        }
    }
}



/// `ChunkStore` is a store of data held as serialised files on disk, implementing a maximum disk
/// usage to restrict storage.
///
/// The data chunks are deleted when the `ChunkStore` goes out of scope.
pub struct ChunkStore<Key, Value> {
    rootdir: PathBuf,
    lock_file: Option<File>,
    max_space: u64,
    used_space: u64,
    workers: HashMap<String, (Arc<AtomicBool>, thread::Joiner)>,
    // <serialised_key, file_name>
    file_name_map: HashMap<Vec<u8>, String>,
    phantom: PhantomData<(Key, Value)>,
}

impl<Key, Value> ChunkStore<Key, Value>
    where Key: Serialize + Deserialize,
          Value: Serialize + Deserialize
{
    /// Creates a new `ChunkStore` with `max_space` allowed storage space.
    ///
    /// The data is stored in a root directory. If `root` doesn't exist, it will be created.
    pub fn new(root: PathBuf, max_space: u64) -> Result<ChunkStore<Key, Value>, Error> {
        let lock_file = Self::lock_and_clear_dir(&root)?;
        Ok(ChunkStore {
               rootdir: root,
               lock_file: Some(lock_file),
               max_space: max_space,
               used_space: 0,
               workers: Default::default(),
               file_name_map: Default::default(),
               phantom: PhantomData,
           })
    }

    fn key_to_name(&self, key: &Key) -> Result<(Vec<u8>, String), Error> {
        let serialised_key = serialisation::serialise(key)?;
        let mut file_name = sha256::hash(serialised_key.as_slice()).0.to_hex();
        file_name.truncate(8);
        Ok((serialised_key, file_name))
    }

    /// Stores a new data chunk under `key`.
    ///
    /// If there is not enough storage space available, returns `Error::NotEnoughSpace`.  In case of
    /// an IO error, it returns `Error::Io`.
    ///
    /// If the key already exists, it will be overwritten.
    pub fn put(&mut self, key: &Key, value: &Value) -> Result<(), Error> {
        self.clean_up_threads(key)?;

        let serialised_value = serialisation::serialise(value)?;
        if self.used_space + serialised_value.len() as u64 > self.max_space {
            return Err(Error::NotEnoughSpace);
        }
        let (serialised_key, file_name) = self.key_to_name(key)?;
        // If a file corresponding to 'key' already exists, delete it.
        let file_path = self.file_path(&file_name)?;
        let _ = self.do_delete(&file_path);
        self.used_space += serialised_value.len() as u64;

        let atomic_completed = Arc::new(AtomicBool::new(false));
        let atomic_completed_clone = atomic_completed.clone();
        let joiner = thread::named("background_put", move || {
            // Write the file.
            let _ = File::create(&file_path)
                .and_then(|mut file| {
                              file.write_all(&serialised_value)
                        .and_then(|()| file.sync_all())
                          });
            atomic_completed_clone.store(true, Ordering::Relaxed);
        });
        let _ = self.workers.insert(file_name.clone(), (atomic_completed, joiner));
        let _ = self.file_name_map.insert(serialised_key, file_name);
        Ok(())
    }

    /// Clean up threads
    ///
    /// Removes completed worker threads from map.
    /// Waits till the specific thread completed, if exists.
    pub fn clean_up_threads(&mut self, key: &Key) -> Result<(), Error> {
        let (_, file_name) = self.key_to_name(key)?;
        let _ = self.workers.remove(&file_name);

        let mut completed_threads = Vec::new();
        for (serialised_key, &(ref atomic_completed, _)) in self.workers.iter() {
            if atomic_completed.load(Ordering::Relaxed) {
                match String::from_str(serialised_key) {
                    Ok(name) => completed_threads.push(name),
                    Err(_) => {}
                }
            }
        }
        for file_name in &completed_threads {
            let _ = self.workers.remove(file_name);
        }
        Ok(())
    }

    /// Deletes the data chunk stored under `key`.
    ///
    /// If the data doesn't exist, it does nothing and returns `Ok`.  In the case of an IO error, it
    /// returns `Error::Io`.
    pub fn delete(&mut self, key: &Key) -> Result<(), Error> {
        self.clean_up_threads(key)?;
        let (serialised_key, file_name) = self.key_to_name(key)?;
        let _ = self.file_name_map.remove(&serialised_key);
        let file_path = self.file_path(&file_name)?;
        self.do_delete(&file_path)
    }

    /// Returns a data chunk previously stored under `key`.
    ///
    /// If the data file can't be accessed, it returns `Error::ChunkNotFound`.
    pub fn get(&self, key: &Key) -> Result<Value, Error> {
        let (_, file_name) = self.key_to_name(key)?;
        match File::open(self.file_path(&file_name)?) {
            Ok(mut file) => {
                let mut contents = Vec::<u8>::new();
                let _ = file.read_to_end(&mut contents)?;
                Ok(serialisation::deserialise::<Value>(&contents)?)
            }
            Err(_) => Err(Error::NotFound),
        }
    }

    /// Tests if a data chunk has been previously stored under `key`.
    pub fn has(&self, key: &Key) -> bool {
        match self.key_to_name(key) {
            Ok((serialised_key, _)) => self.file_name_map.contains_key(&serialised_key),
            Err(_) => false,
        }        
    }

    /// Lists all keys of currently-data stored.
    pub fn keys(&self) -> Vec<Key> {
        self.file_name_map.keys().filter_map(|serialised_key|
            serialisation::deserialise::<Key>(serialised_key.as_slice()).ok()
        ).collect()
    }

    /// Returns the maximum amount of storage space available for this ChunkStore.
    pub fn max_space(&self) -> u64 {
        self.max_space
    }

    /// Returns the amount of storage space already used by this ChunkStore.
    pub fn used_space(&self) -> u64 {
        self.used_space
    }

    /// Creates and clears the given root directory and returns a locked file inside it.
    fn lock_and_clear_dir(root: &PathBuf) -> Result<File, Error> {
        // Create the chunk directory and a lock file.
        fs::create_dir_all(&root)?;
        let lock_file_path = root.join(LOCK_FILE_NAME);
        let lock_file = File::create(&lock_file_path)?;
        lock_file.try_lock_exclusive()?;

        // Verify that chunk files can be created.
        let name: String = (0..MAX_CHUNK_FILE_NAME_LENGTH).map(|_| '0').collect();
        let _ = File::create(&root.join(name))?;

        // Clear the chunk directory.
        for entry_result in fs::read_dir(&root)? {
            let entry = entry_result?;
            if entry.path() != lock_file_path.as_path() {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(lock_file)
    }

    fn do_delete(&mut self, file_path: &Path) -> Result<(), Error> {
        if let Ok(metadata) = fs::metadata(file_path) {
            self.used_space -= cmp::min(metadata.len(), self.used_space);
            fs::remove_file(file_path).map_err(From::from)
        } else {
            Ok(())
        }
    }

    fn file_path(&self, file_name: &String) -> Result<PathBuf, Error> {
        let path_name = Path::new(file_name);
        Ok(self.rootdir.join(path_name))
    }
}

impl<Key, Value> Drop for ChunkStore<Key, Value> {
    fn drop(&mut self) {
        let _ = self.lock_file.take().iter().map(File::unlock);
        let _ = fs::remove_dir_all(&self.rootdir);
    }
}

#[cfg(test)]
mod test;
