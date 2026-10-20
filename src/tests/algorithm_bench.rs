// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! This file also compiles unchanged against the pre-optimization revision.
use crate::nodes::leaf_node::{LeafNode, LeafNodeHeader, MiniPageNextLevel, OpType};
use std::{hint::black_box, time::Instant};

struct Page(*mut LeafNodeHeader);
impl Page {
    fn new() -> Self {
        Self(LeafNode::make_base_page(4096, 0))
    }
    fn leaf(&mut self) -> &mut LeafNode {
        unsafe { &mut *LeafNode::from_raw_parts(self.0, 4096) }
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        let layout =
            std::alloc::Layout::from_size_align(4096, std::mem::align_of::<LeafNodeHeader>())
                .unwrap();
        unsafe { std::alloc::dealloc(self.0.cast(), layout) };
    }
}

#[test]
#[ignore = "manual algorithm benchmark"]
fn leaf_merge_microbench() {
    let samples: usize = std::env::var("BFTREE_BENCH_SAMPLES")
        .unwrap_or_else(|_| "9".into())
        .parse()
        .unwrap();
    const ITERATIONS: usize = 512;
    println!("workload,median_ns_per_op,min_ns_per_op,max_ns_per_op");
    for (key_len, count) in [(8, 64u64), (128, 12)] {
        for dirty in [1, 4, count.min(16)] {
            let key = |id: u64| {
                let mut key = vec![b'p'; key_len];
                key[key_len - 8..].copy_from_slice(&id.to_be_bytes());
                key
            };
            for kind in ["insert", "overwrite"] {
                let mut times = Vec::new();
                for sample in 0..=samples {
                    let mut pages: Vec<_> = (0..ITERATIONS)
                        .map(|_| {
                            let mut page = Page::new();
                            for id in 0..count {
                                assert!(page.leaf().insert(
                                    &key(id * 2),
                                    &[1; 16],
                                    OpType::Insert,
                                    0
                                ));
                            }
                            page
                        })
                        .collect();
                    let mut delta = Page::new();
                    let mini = delta.leaf();
                    mini.initialize(&[], &[], 4096, MiniPageNextLevel::new(0), false, false, 1);
                    for id in 0..dirty {
                        let id = if kind == "insert" { id * 2 + 1 } else { id * 2 };
                        assert!(mini.insert(&key(id), &[2; 16], OpType::Insert, 0));
                    }
                    let start = Instant::now();
                    for page in &mut pages {
                        black_box(page.leaf().merge_mini_page(black_box(mini), 0));
                    }
                    let elapsed = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
                    // Verify every result outside timing, including the success of the merge.
                    for page in &mut pages {
                        let leaf = page.leaf();
                        let expected = count + if kind == "insert" { dirty } else { 0 };
                        assert_eq!(leaf.meta.meta_count_without_fence() as u64, expected);
                        for id in 0..dirty {
                            let id = if kind == "insert" { id * 2 + 1 } else { id * 2 };
                            let pos = leaf.lower_bound(&key(id));
                            let meta = leaf.get_kv_meta(pos as usize);
                            assert_eq!(leaf.get_full_key(meta), key(id));
                            assert_eq!(leaf.get_value(meta), &[2; 16]);
                        }
                    }
                    if sample != 0 {
                        times.push(elapsed);
                    }
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "merge_{kind}/k{key_len}/m{dirty},{:.3},{:.3},{:.3}",
                    times[samples / 2],
                    times[0],
                    times[samples - 1]
                );
            }
        }
    }
}

/// Success-path comparison for the two optimized implementations. The original
/// baseline rejects some of these fragmented pages, so its failure time is not
/// comparable to a successful merge and is deliberately not measured here.
#[test]
#[ignore = "manual fragmented-page benchmark"]
fn leaf_growth_microbench() {
    let samples: usize = std::env::var("BFTREE_BENCH_SAMPLES")
        .unwrap_or_else(|_| "9".into())
        .parse()
        .unwrap();
    assert!(samples > 0);
    const ITERATIONS: usize = 512;
    println!("workload,median_ns_per_op,min_ns_per_op,max_ns_per_op");
    for key_len in [8, 128] {
        for prefixed in [false, true] {
            for dirty in [1u64, 4] {
                let key = |id: u64| {
                    let mut key = vec![b'p'; key_len];
                    key[key_len - 8..].copy_from_slice(&id.to_be_bytes());
                    key
                };
                let low = key(0);
                let high = key(u64::MAX);
                let mut times = Vec::new();
                for sample in 0..=samples {
                    let mut count = 0u64;
                    let mut pages: Vec<_> = (0..ITERATIONS)
                        .map(|_| {
                            let mut page = Page::new();
                            let leaf = page.leaf();
                            if prefixed {
                                leaf.initialize(
                                    &low,
                                    &high,
                                    4096,
                                    MiniPageNextLevel::new_null(),
                                    true,
                                    false,
                                    0,
                                );
                            }
                            let mut id = 0;
                            while leaf.insert(&key(id), &[1; 128], OpType::Insert, 2 * key_len) {
                                id += 1;
                            }
                            count = id;
                            // Leave fragmented payloads from larger old values.
                            for id in 0..count {
                                assert!(leaf.insert(&key(id), &[1; 32], OpType::Insert, 0));
                            }
                            page
                        })
                        .collect();
                    assert!(count > dirty);
                    let mut deltas: Vec<_> = [64usize, 96, 128, 32]
                        .into_iter()
                        .enumerate()
                        .map(|(pass, len)| {
                            let mut delta = Page::new();
                            let mini = delta.leaf();
                            mini.initialize(
                                &[],
                                &[],
                                4096,
                                MiniPageNextLevel::new(0),
                                false,
                                false,
                                pass as u64 + 1,
                            );
                            for id in 0..dirty {
                                assert!(mini.insert(
                                    &key(id * 2),
                                    &vec![pass as u8 + 2; len],
                                    OpType::Insert,
                                    0
                                ));
                            }
                            delta
                        })
                        .collect();
                    let mut succeeded = 0;
                    let start = Instant::now();
                    for delta in &mut deltas {
                        let mini = delta.leaf();
                        for page in &mut pages {
                            succeeded += usize::from(black_box(
                                page.leaf().merge_mini_page(black_box(mini), 2 * key_len),
                            ));
                        }
                    }
                    let elapsed =
                        start.elapsed().as_nanos() as f64 / (ITERATIONS * deltas.len()) as f64;
                    assert_eq!(succeeded, ITERATIONS * deltas.len());
                    for page in &mut pages {
                        let leaf = page.leaf();
                        assert_eq!(leaf.meta.meta_count_without_fence() as u64, count);
                        assert_eq!(leaf.get_clean_snapshot_version(), 4);
                        for id in 0..count {
                            let pos = leaf.lower_bound(&key(id));
                            let meta = leaf.get_kv_meta(pos as usize);
                            assert_eq!(leaf.get_full_key(meta), key(id));
                            let byte = if id % 2 == 0 && id / 2 < dirty { 5 } else { 1 };
                            assert_eq!(leaf.get_value(meta), &[byte; 32]);
                        }
                    }
                    if sample != 0 {
                        times.push(elapsed);
                    }
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "growth/k{key_len}/prefix{prefixed}/m{dirty},{:.3},{:.3},{:.3}",
                    times[samples / 2],
                    times[0],
                    times[samples - 1]
                );
            }
        }
    }
}
