// Copyright 2017 MaidSafe.net limited.
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

// For explanation of lint checks, run `rustc -W help` or see
// https://github.com/maidsafe/QA/blob/master/Documentation/Rust%20Lint%20Checks.md

#![forbid(bad_style, exceeding_bitshifts, mutable_transmutes, no_mangle_const_items,
          unknown_crate_types, warnings)]
#![deny(deprecated, improper_ctypes, missing_docs,
        non_shorthand_field_patterns, overflowing_literals, plugin_as_library,
        private_no_mangle_fns, private_no_mangle_statics, stable_features, unconditional_recursion,
        unknown_lints, unsafe_code, unused, unused_allocation, unused_attributes,
        unused_comparisons, unused_features, unused_parens, while_true)]
#![warn(trivial_casts, trivial_numeric_casts, unused_extern_crates, unused_import_braces,
        unused_qualifications, unused_results)]
#![allow(box_pointers, fat_ptr_transmutes, missing_copy_implementations,
         missing_debug_implementations, variant_size_differences)]

#![feature(test)]

extern crate rand;
extern crate safe_vault;
extern crate tempdir;
extern crate test;
#[macro_use]
extern crate unwrap;

use rand::Rng;
use safe_vault::chunk_store::ChunkStore;
use tempdir::TempDir;
use test::Bencher;

fn generate_random_bytes(size: u64) -> Vec<u8> {
    rand::thread_rng()
        .gen_iter()
        .take(size as usize)
        .collect()
}

#[bench]
fn bench_write(b: &mut Bencher) {
    let one_mb = 1024 * 1024;
    let data = generate_random_bytes(one_mb);
    let root = unwrap!(TempDir::new("test"));
    // 1MB (1048576) random data will have 1048584 serialised size.
    let mut chunk_store = unwrap!(ChunkStore::new(root.path().to_path_buf(), 1024 * one_mb));
    b.iter(|| unwrap!(chunk_store.put(&1000, &data)));
}

#[bench]
fn bench_read(b: &mut Bencher) {
    let one_mb = 1024 * 1024;
    let nums = 100;
    let data = generate_random_bytes(one_mb);
    let root = unwrap!(TempDir::new("test"));
    // 1MB (1048576) random data will have 1048584 serialised size.
    let mut chunk_store = unwrap!(ChunkStore::new(root.path().to_path_buf(), nums * (one_mb + 8)));
    for i in 0..nums as usize {
        unwrap!(chunk_store.put(&i, &data));
    }

    b.iter(|| {
               let _ = unwrap!(chunk_store.get(&rand::thread_rng().gen_range(0, nums as usize)));
           });
}
