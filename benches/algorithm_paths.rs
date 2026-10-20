// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Public-API merge/scan workloads, with setup and validation outside timing.
//! Memory-backed pages exercise the disk algorithm without device latency.
//! Std files use the OS page cache; these are not durable-storage benchmarks.
use bf_tree::{BfTree, Config, LeafInsertResult, LeafReadResult, ScanReturnField, StorageBackend};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};
use std::{hint::black_box, time::Instant};

const RECORDS: usize = 4096;
const PASSES: usize = 4;

fn main() {
    let samples = std::env::var("BFTREE_BENCH_SAMPLES")
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(9);
    assert!(samples > 0);
    let filter = std::env::var("BFTREE_BENCH_FILTER").unwrap_or_default();
    // Diagnostic runs split the phases; leave this off for normal comparisons.
    let phases = std::env::var_os("BFTREE_BENCH_PHASES").is_some();
    let mut order: Vec<_> = (0..RECORDS).collect();
    order.shuffle(&mut StdRng::seed_from_u64(0xA190_2026));
    println!("workload,median_ns_per_op,min_ns_per_op,max_ns_per_op");
    for key_len in [8, 128] {
        let keys: Vec<_> = (0..RECORDS)
            .map(|i| {
                let mut key = vec![b'p'; key_len];
                key[key_len - 8..].copy_from_slice(&(i as u64).to_be_bytes());
                key
            })
            .collect();
        for backend in ["cache", "paged_memory", "file_buffered"] {
            for stride in [1, 16] {
                let name = format!("update_scan/{backend}/k{key_len}/stride{stride}");
                if !name.contains(&filter) {
                    continue;
                }
                let mut times = Vec::new();
                let mut update_times = Vec::new();
                let mut scan_times = Vec::new();
                for sample in 0..=samples {
                    let directory = tempfile::tempdir().unwrap();
                    let mut config = Config::default();
                    config
                        .cache_only(backend == "cache")
                        .storage_backend(if backend == "file_buffered" {
                            StorageBackend::Std
                        } else {
                            StorageBackend::Memory
                        })
                        .file_path(directory.path().join("pages.bft"))
                        .cb_size_byte(8 * 1024 * 1024)
                        .cb_max_key_len(key_len)
                        .cb_max_record_size(1024)
                        .write_load_full_page(false)
                        .read_promotion_rate(0)
                        .scan_promotion_rate(0);
                    let tree = BfTree::with_config(config, None).unwrap();
                    for &i in &order {
                        assert_eq!(tree.insert(&keys[i], &[0; 32]), LeafInsertResult::Success);
                    }
                    // Normalize base pages before measuring the next deltas.
                    let mut out = vec![0; key_len + 128];
                    let mut scan = tree
                        .scan_with_count(&keys[0], RECORDS, ScanReturnField::KeyAndValue)
                        .unwrap();
                    let mut count = 0;
                    while scan.next(&mut out).is_some() {
                        count += 1;
                    }
                    assert_eq!(count, RECORDS);
                    drop(scan);
                    let values: Vec<_> = [64, 96, 128, 32]
                        .into_iter()
                        .enumerate()
                        .map(|(i, len)| vec![i as u8 + 1; len])
                        .collect();
                    let mut total_records = 0;
                    let mut update_elapsed = std::time::Duration::ZERO;
                    let mut scan_elapsed = std::time::Duration::ZERO;
                    #[cfg(feature = "metrics-rt")]
                    bf_tree::metric::get_tls_recorder().reset();
                    let start = Instant::now();
                    for value in &values {
                        let phase_start = phases.then(Instant::now);
                        for &i in order.iter().step_by(stride) {
                            black_box(tree.insert(black_box(&keys[i]), black_box(value)));
                        }
                        if let Some(start) = phase_start {
                            update_elapsed += start.elapsed();
                        }
                        let phase_start = phases.then(Instant::now);
                        let mut scan = tree
                            .scan_with_count(&keys[0], RECORDS, ScanReturnField::KeyAndValue)
                            .unwrap();
                        while let Some(result) = scan.next(black_box(&mut out)) {
                            black_box(result);
                            total_records += 1;
                        }
                        if let Some(start) = phase_start {
                            scan_elapsed += start.elapsed();
                        }
                    }
                    // Unit: one updated record or one returned scan record.
                    let elapsed = start.elapsed().as_nanos() as f64
                        / (PASSES * (RECORDS + RECORDS / stride)) as f64;
                    #[cfg(feature = "metrics-rt")]
                    if phases && sample == 0 {
                        eprintln!(
                            "{name}: {}",
                            serde_json::to_string(bf_tree::metric::get_tls_recorder()).unwrap()
                        );
                    }
                    assert_eq!(total_records, RECORDS * PASSES);
                    let mut changed = vec![false; RECORDS];
                    for &i in order.iter().step_by(stride) {
                        changed[i] = true;
                    }
                    let mut scan = tree
                        .scan_with_count(&keys[0], RECORDS + 1, ScanReturnField::KeyAndValue)
                        .unwrap();
                    for (i, key) in keys.iter().enumerate() {
                        assert_eq!(scan.next(&mut out), Some((key_len, 32)));
                        assert_eq!(&out[..key_len], key);
                        assert_eq!(
                            &out[key_len..key_len + 32],
                            &[if changed[i] { 4 } else { 0 }; 32]
                        );
                    }
                    assert_eq!(scan.next(&mut out), None);
                    drop(scan);
                    for (i, key) in keys.iter().enumerate() {
                        assert_eq!(tree.read(key, &mut out), LeafReadResult::Found(32));
                        assert_eq!(&out[..32], &[if changed[i] { 4 } else { 0 }; 32]);
                    }
                    if sample != 0 {
                        times.push(elapsed);
                        update_times.push(
                            update_elapsed.as_nanos() as f64 / (PASSES * RECORDS / stride) as f64,
                        );
                        scan_times.push(scan_elapsed.as_nanos() as f64 / (PASSES * RECORDS) as f64);
                    }
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "{name},{:.3},{:.3},{:.3}",
                    times[samples / 2],
                    times[0],
                    times[samples - 1]
                );
                if phases {
                    for (phase, mut values) in [("update", update_times), ("scan", scan_times)] {
                        values.sort_by(f64::total_cmp);
                        println!(
                            "{phase}_phase/{backend}/k{key_len}/stride{stride},{:.3},{:.3},{:.3}",
                            values[samples / 2],
                            values[0],
                            values[samples - 1]
                        );
                    }
                }
            }
        }
    }
}
