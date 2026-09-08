// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, TestRunner};
use proptest_derive::Arbitrary;
use std::collections::{BTreeMap, HashMap};

use crate::nodes::leaf_node::{LeafNode, LeafReadResult, MiniPageNextLevel, OpType};

struct TestBasePage(*mut LeafNode);

impl TestBasePage {
    fn new(size: usize) -> Self {
        Self(LeafNode::make_base_page(
            size,
            crate::snapshot::INVALID_SNAPSHOT_VERSION,
        ))
    }

    fn page(&mut self) -> &mut LeafNode {
        // This guard owns the allocation, and the returned reference is tied to it.
        unsafe { &mut *self.0 }
    }
}

impl Drop for TestBasePage {
    fn drop(&mut self) {
        LeafNode::free_base_page(self.0);
    }
}

#[derive(Clone, Arbitrary, Debug)]
enum LeafTestOp {
    Insert,
    Delete,
    Read,
}

fn leaf_insert_read(input: Vec<(Vec<u8>, Vec<u8>, LeafTestOp)>) {
    let mut model = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut allocation = TestBasePage::new(4096);
    let leaf = allocation.page();
    let mut out_buffer = vec![0u8; 1024]; // Buffer for reading from LeafNode

    for (k, v, op) in input.iter() {
        match op {
            LeafTestOp::Insert => {
                let rt = leaf.insert(k, v, OpType::Insert, 60);
                assert!(rt);

                model.insert(k.to_owned(), v.to_owned());
            }
            LeafTestOp::Delete => {
                let _ = leaf.insert(k, &[], OpType::Delete, 60);
                model.remove(k);
            }
            LeafTestOp::Read => {
                let rt = leaf.read_by_key(k, &mut out_buffer);
                match model.get(k) {
                    Some(v) => {
                        assert_eq!(rt, LeafReadResult::Found(v.len() as u32));
                        assert_eq!(&out_buffer[0..v.len()], v);
                    }
                    None => {
                        assert!(rt == LeafReadResult::NotFound || rt == LeafReadResult::Deleted);
                    }
                }
            }
        }
    }

    let model_cnt = model.len();
    // Now sanity check every value
    for (k, v) in model {
        let rt = leaf.read_by_key(&k, &mut out_buffer);
        assert_eq!(rt, LeafReadResult::Found(v.len() as u32));
        if &out_buffer[0..v.len()] != v {
            let rt = leaf.read_by_key(&k, &mut out_buffer);
            assert_eq!(rt, LeafReadResult::Found(v.len() as u32));
        }
        assert_eq!(&out_buffer[0..v.len()], v);
    }

    leaf.consolidate(crate::snapshot::INVALID_SNAPSHOT_VERSION);
    let leaf_cnt = leaf.meta.meta_count_without_fence();
    assert_eq!(model_cnt, leaf_cnt as usize);
}

fn collision_key(prefix: &[u8], id: u8) -> Vec<u8> {
    let mut key = prefix.to_vec();
    match id {
        0 => {}
        1 => key.extend_from_slice(&[0]),
        2 => key.extend_from_slice(&[0, 0]),
        3 => key.extend_from_slice(&[0, 0, 0]),
        4 => key.extend_from_slice(&[0, 0, 1]),
        5 => key.extend_from_slice(&[0, 1]),
        6 => key.extend_from_slice(&[1]),
        7 => key.extend_from_slice(&[1, 0]),
        _ => key.extend_from_slice(&[2, 2, id]),
    }
    key
}

proptest! {
    #![proptest_config(Config::with_cases(256))]

    #[test]
    fn leaf_search_update_and_consolidation_match_model(
        prefix in proptest::collection::vec(any::<u8>(), 0..32),
        operations in proptest::collection::vec(
            (0u8..16, proptest::collection::vec(any::<u8>(), 1..80), 0u8..4),
            1..64,
        ),
    ) {
        let mut allocation = TestBasePage::new(8192);
        let leaf = allocation.page();
        let mut high_fence = prefix.clone();
        high_fence.push(u8::MAX);
        leaf.initialize(
            &prefix,
            &high_fence,
            8192,
            MiniPageNextLevel::new_null(),
            true,
            false,
            crate::snapshot::INVALID_SNAPSHOT_VERSION,
        );
        let mut model = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        let mut out = [0u8; 80];
        for (id, value, operation) in operations {
            let key = collision_key(&prefix, id);
            if operation == 0 {
                prop_assert!(leaf.insert(&key, &[], OpType::Delete, 0));
                model.remove(&key);
            } else {
                prop_assert!(leaf.insert(&key, &value, OpType::Insert, 0));
                model.insert(key, value);
            }

            // Small key spaces deliberately force overwrite, growth, deletion,
            // resurrection, and collisions in the two-byte metadata preview.
            for query_id in 0..17 {
                let query = collision_key(&prefix, query_id);
                let binary = leaf.read_by_key_inner(&query, &mut out, true);
                if let Some(expected) = model.get(&query) {
                    prop_assert_eq!(&binary, &LeafReadResult::Found(expected.len() as u32));
                    prop_assert_eq!(&out[..expected.len()], expected);
                } else {
                    prop_assert!(matches!(binary, LeafReadResult::NotFound | LeafReadResult::Deleted));
                }
                let linear = leaf.read_by_key_inner(&query, &mut out, false);
                prop_assert_eq!(binary, linear);
                if let Some(expected) = model.get(&query) {
                    prop_assert_eq!(&out[..expected.len()], expected);
                }
            }
        }

        let mut queries: Vec<_> = (0..17).map(|id| collision_key(&prefix, id)).collect();
        queries.extend((0..prefix.len()).map(|len| prefix[..len].to_vec()));
        queries.extend([vec![0], vec![u8::MAX], high_fence.clone()]);
        let stored: Vec<_> = leaf.meta_iter().map(|meta| leaf.get_full_key(meta)).collect();
        for query in queries {
            let expected = 2 + stored.partition_point(|key| key < &query) as u16;
            prop_assert_eq!(leaf.lower_bound(&query), expected);
            prop_assert_eq!(leaf.linear_lower_bound(&query), expected);
        }

        leaf.consolidate(37);
        prop_assert_eq!(leaf.get_low_fence_key(), prefix);
        prop_assert_eq!(leaf.get_high_fence_key(), high_fence);
        prop_assert_eq!(leaf.get_clean_snapshot_version(), 37);
        prop_assert_eq!(leaf.meta.meta_count_without_fence() as usize, model.len());
        for (meta, (key, value)) in leaf.meta_iter().zip(&model) {
            prop_assert_eq!(&leaf.get_full_key(meta), key);
            prop_assert_eq!(leaf.get_value(meta), value);
            prop_assert_eq!(meta.op_type(), OpType::Insert);
            prop_assert!(!meta.is_referenced());
        }
    }
}

#[test]
fn leaf_consolidation_changes_prefix_and_skips_requested_key() {
    let mut allocation = TestBasePage::new(4096);
    let leaf = allocation.page();
    leaf.initialize(
        b"tenant/a",
        b"tenant/z",
        4096,
        MiniPageNextLevel::new_null(),
        true,
        false,
        crate::snapshot::INVALID_SNAPSHOT_VERSION,
    );
    assert!(leaf.insert(b"tenant/aa", b"short", OpType::Insert, 0));
    assert!(leaf.insert(b"tenant/ab", &[3; 96], OpType::Insert, 0));
    assert!(leaf.insert(b"tenant/ac", b"delete", OpType::Insert, 0));
    assert!(leaf.insert(b"tenant/ac", &[], OpType::Delete, 0));
    assert!(leaf.insert(b"tenant/ad", b"skip", OpType::Insert, 0));
    leaf.lsn = 42;
    leaf.consolidate_inner(
        OpType::Insert,
        Some(b"tenant/az"),
        true,
        false,
        Some(b"tenant/ad"),
        73,
    );

    assert_eq!(leaf.get_prefix(), b"tenant/a");
    assert_eq!(leaf.get_low_fence_key(), b"tenant/a");
    assert_eq!(leaf.get_high_fence_key(), b"tenant/az");
    assert_eq!(leaf.meta.meta_count_without_fence(), 2);
    assert_eq!(leaf.lsn, 42);
    assert_eq!(leaf.get_clean_snapshot_version(), 73);
    let mut out = [0; 96];
    assert_eq!(
        leaf.read_by_key(b"tenant/aa", &mut out),
        LeafReadResult::Found(5)
    );
    assert_eq!(&out[..5], b"short");
    assert_eq!(
        leaf.read_by_key(b"tenant/ab", &mut out),
        LeafReadResult::Found(96)
    );
    assert_eq!(out, [3; 96]);
    for key in [b"tenant/ac", b"tenant/ad"] {
        assert_eq!(leaf.read_by_key(key, &mut out), LeafReadResult::NotFound);
    }
}

#[test]
fn leaf_insert_rejects_unrepresentable_lengths_without_mutation() {
    let mut allocation = TestBasePage::new(4096);
    let leaf = allocation.page();
    leaf.initialize(
        b"tenant/a",
        b"tenant/z",
        4096,
        MiniPageNextLevel::new_null(),
        true,
        false,
        0,
    );
    assert!(leaf.insert(b"tenant/m", b"original", OpType::Insert, 0));
    let remaining = leaf.meta.remaining_size;
    let count = leaf.meta.meta_count_with_fence();

    for len in [1 << 14, 1 << 16, (1 << 16) + 8] {
        assert!(!leaf.insert(&vec![b'a'; len], b"value", OpType::Insert, 0));
    }
    for len in [1 << 15, 1 << 16, (1 << 16) + 8] {
        assert!(!leaf.insert(b"tenant/m", &vec![1; len], OpType::Insert, 0));
    }
    assert!(!leaf.insert(b"short", b"value", OpType::Insert, 0));

    assert_eq!(leaf.meta.remaining_size, remaining);
    assert_eq!(leaf.meta.meta_count_with_fence(), count);
    let mut out = [0; 8];
    assert_eq!(
        leaf.read_by_key(b"tenant/m", &mut out),
        LeafReadResult::Found(8)
    );
    assert_eq!(&out, b"original");
}

#[test]
fn leaf_base_page_unused_storage_is_initialized() {
    let allocation = TestBasePage::new(4096);
    // Whole pages are serialized by storage. This read also lets Miri check
    // that the header, fence metadata, and unused capacity are initialized.
    let bytes = unsafe { std::slice::from_raw_parts(allocation.0.cast::<u8>(), 4096) };
    let unused_start = std::mem::size_of::<LeafNode>() + 2 * crate::nodes::KV_META_SIZE;
    assert!(bytes[unused_start..].iter().all(|byte| *byte == 0));
}

#[test]
fn leaf_empty_prefix_and_empty_consolidation_are_valid() {
    let mut allocation = TestBasePage::new(4096);
    let leaf = allocation.page();
    assert_eq!(leaf.lsn, 0);
    assert!(leaf.get_prefix().is_empty());
    leaf.consolidate(1);
    assert_eq!(leaf.meta.meta_count_without_fence(), 0);

    // A fresh fence-less page has no initialized record metadata to inspect.
    leaf.initialize(
        &[],
        &[],
        4096,
        MiniPageNextLevel::new_null(),
        false,
        false,
        2,
    );
    assert!(leaf.get_prefix().is_empty());
    leaf.consolidate(3);
    assert_eq!(leaf.meta.meta_count_without_fence(), 0);
    assert!(leaf.get_prefix().is_empty());
}

#[test]
fn test_leaf_insert_read() {
    let config = Config {
        cases: 1000,
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        source_file: Some("src/prop_tests/leaf_node.rs"),
        ..Config::default()
    };

    let strategy = proptest::collection::vec(
        (
            proptest::collection::vec(any::<u8>(), 1..30), // Key
            proptest::collection::vec(any::<u8>(), 1..30), // Value
            any::<LeafTestOp>(),
        ),
        1..50, // Length of the list
    );

    let test = |input: Vec<(Vec<u8>, Vec<u8>, LeafTestOp)>| {
        leaf_insert_read(input);
        Ok(())
    };

    let mut runner = TestRunner::new(config);
    runner.run(&strategy, test).unwrap();
}
