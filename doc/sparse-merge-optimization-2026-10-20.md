# 2026-10-20 长键稀疏更新退化的后续优化

本轮针对[上一轮报告](algorithm-optimization-2026-10-20.md)中的
`update_scan/paged_memory/k128/stride16` 退化。最终版本在本轮配对测量中，
从上一版 **42.55 ns 降至 39.67 ns，耗时下降 6.78%**；原始基线为 41.85 ns，
相对基线下降 **5.22%**。两个对照均为 6/6 轮更快。
这些数字来自同一轮实验，不把历史报告中的 40.15/42.29 ns 与本轮混算。

## 最终实现

修改只进一步优化 `merge_mini_page_sorted` 的整页重建路径：

1. 新旧基础页拥有相同 fence 和公共前缀，未变记录直接复制已存储的
   **键后缀＋值**，重新写入偏移及元数据。省去完整键缓冲区、公共前缀复制和重复拆分。
2. 两路有序归并按键后缀比较；页范围保证脏键共享公共前缀，
   Debug 检查这一不变量，避免为每一条基础记录重复比较长前缀。
3. 保留独立临时页及成功后发布的语义，容量不足时原页不变；
   复制后的记录清除 reference bit，保留正确的键预览、LSN、fence 和快照版本。

复杂度仍为 O(n+m)，本次主要降低比较、复制和缓冲区管理的常数开销。
公共 API、页布局、磁盘格式及并发协议不变。实现见
[leaf_node.rs](../src/nodes/leaf_node.rs)。

诊断还确认了精确容量判断可能重复查找、保守预估没有扣除公共前缀，
但本轮没有保留这些路径的修改。试验中的前缀容量调整和禁止内联设置未显示
足够稳定的综合收益；最终保留上述归并优化，未改变原地更新策略及归并阈值。

## 对照和复现条件

- `baseline`：原始提交 `6fd826fb1a3a4a29cfa0902428428376449b667d`。
- `previous`：本轮开始前的上一版工作区源码，生产源码 SHA-256 与上一轮清单一致。
- `candidate`：上述后缀归并优化后的源码。
- 相同机器、Rust 1.98.1 / MSVC、Cargo.lock 和普通 Release 配置；未启用 LTO、PGO 或 native。
- i7-13700KF，Windows 11 `10.0.26220`；固定逻辑 CPU 4，affinity mask 16。
- 三个版本独立构建并冻结二进制；测量期间无本任务的编译、测试和 Miri。
- 6 轮，每个进程 1 次预热、9 个计时样本；三个版本遍历全部六种运行顺序，
  每个版本处于每个位置各两次。增长微基准仅比较 previous/candidate，交替运行顺序。
- 表中为六个进程中位数的中位数（偶数时平均中间两项），
  耗时下降 = `100 × (1 − candidate / reference)`。全部不利样本保留。
- 原有 39 项负载定义沿用上一轮。`algorithm_paths` 在三个版本中采用相同驱动；
  新增分段计时开关在正式测量中关闭。
- `file_buffered` 仍为操作系统页缓存下的普通文件 I/O，不是 fsync、冷盘或持久化性能；
  全部为单线程测量，不外推并发吞吐或 p99。

新增 8 项 `growth` 微基准：4 KiB 页先以 128B 值填充，再原地缩短到 32B 形成碎片；
1 或 4 条记录随后按 64/96/128/32B 连续更新，每次都合并。
每样本 512 个独立页，输入构造和全部结果校验在计时区外。
单位为 ns/一次合并，包含重建分配与释放；每次合并必须成功。
`prefixtrue` 表示使用有限 fence（128B 键压缩 120B 公共前缀，8B 键公共前缀为 0）；
`prefixfalse` 使用无限 fence。原始版本会拒绝部分碎片页，因此不将它的失败耗时
与这里成功完成的合并混算。

## 结果及仍存在的不利项

- 目标退化项：比上一版快 6.78%，比原始基线快 5.22%，均为 6/6 轮更快。
- 四项文件后端组合负载：比原始基线快 21.58%～38.21%。
- 原有 12 项页级合并：比原始基线快 22.52%～53.87%，全部为 6/6 轮更快。
- 新增碎片页增长负载：比上一版快 11.85%～33.46%，7 项为 6/6 更快，1 项为 5/6 更快。
- 普通 cache-only 扫描相对原始基线慢 0.05%～0.33%，每项均为 3/6 轮更快，
  没有复现中间候选明显的扫描回退；不主张这几项获得确定收益。
- **两项单条长键微基准仍回退**：`merge_insert/k128/m1` 比上一版慢 7.81%
  （1/6 更快），`merge_overwrite/k128/m1` 慢 7.61%（2/6 更快）。
  相对原始基线仍分别快 35.04% 和 36.53%。两项输入均走原地路径，
  未执行本次修改的整页重建分支，具体原因尚未定位，不能宣称所有场景都有改善。

### 更新＋扫描

这里的 ns 是每次更新或每条扫描返回记录的组合平均耗时，不能解释成单独更新延迟。

| 负载 | 原始基线 ns | 上一版 ns | 本版 ns | 对基线耗时下降 | 对上一版耗时下降 | 更快轮数（基线 / 上一版） |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| update_scan/cache/k128/stride1 | 193.76 | 199.30 | 192.96 | 0.41% | 3.18% | 3/6 / 6/6 |
| update_scan/cache/k128/stride16 | 44.69 | 44.30 | 43.81 | 1.98% | 1.12% | 6/6 / 6/6 |
| update_scan/cache/k8/stride1 | 166.55 | 173.28 | 164.17 | 1.43% | 5.26% | 4/6 / 6/6 |
| update_scan/cache/k8/stride16 | 33.99 | 34.87 | 33.94 | 0.14% | 2.66% | 4/6 / 6/6 |
| update_scan/file_buffered/k128/stride1 | 773.53 | 503.19 | 477.96 | 38.21% | 5.01% | 6/6 / 6/6 |
| update_scan/file_buffered/k128/stride16 | 144.29 | 114.13 | 113.15 | 21.58% | 0.86% | 6/6 / 4/6 |
| update_scan/file_buffered/k8/stride1 | 457.63 | 337.52 | 324.15 | 29.17% | 3.96% | 6/6 / 6/6 |
| update_scan/file_buffered/k8/stride16 | 132.57 | 97.58 | 94.27 | 28.89% | 3.39% | 6/6 / 6/6 |
| update_scan/paged_memory/k128/stride1 | 277.21 | 225.78 | 204.18 | 26.35% | 9.57% | 6/6 / 6/6 |
| update_scan/paged_memory/k128/stride16 | 41.85 | 42.55 | 39.67 | 5.22% | 6.78% | 6/6 / 6/6 |
| update_scan/paged_memory/k8/stride1 | 211.06 | 180.64 | 170.80 | 19.07% | 5.45% | 6/6 / 6/6 |
| update_scan/paged_memory/k8/stride16 | 42.83 | 34.42 | 32.06 | 25.14% | 6.86% | 6/6 / 6/6 |

### 原有 cache-only 负载

| 负载 | 原始基线 ns | 上一版 ns | 本版 ns | 对基线耗时下降 | 对上一版耗时下降 | 更快轮数（基线 / 上一版） |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| insert/k128 | 422.80 | 427.28 | 428.85 | -1.43% | -0.37% | 2/6 / 3/6 |
| insert/k32 | 318.95 | 319.36 | 314.93 | 1.26% | 1.38% | 4/6 / 4/6 |
| insert/k8 | 275.45 | 280.26 | 279.18 | -1.35% | 0.39% | 2/6 / 4/6 |
| read_hit/k128 | 263.01 | 263.43 | 263.73 | -0.27% | -0.11% | 3/6 / 4/6 |
| read_hit/k32 | 200.79 | 201.52 | 199.28 | 0.75% | 1.12% | 4/6 / 2/6 |
| read_hit/k8 | 175.45 | 175.62 | 176.86 | -0.80% | -0.70% | 2/6 / 3/6 |
| read_miss/k128 | 270.98 | 275.04 | 273.59 | -0.96% | 0.53% | 1/6 / 3/6 |
| read_miss/k32 | 207.82 | 209.74 | 207.20 | 0.30% | 1.21% | 3/6 / 3/6 |
| read_miss/k8 | 184.90 | 182.20 | 184.32 | 0.32% | -1.16% | 3/6 / 2/6 |
| scan/k128 | 28.23 | 28.33 | 28.32 | -0.33% | 0.02% | 3/6 / 3/6 |
| scan/k32 | 15.08 | 15.31 | 15.09 | -0.05% | 1.45% | 3/6 / 3/6 |
| scan/k8 | 12.49 | 12.81 | 12.50 | -0.08% | 2.45% | 3/6 / 3/6 |
| update_grow/k128 | 482.78 | 483.71 | 482.56 | 0.05% | 0.24% | 3/6 / 4/6 |
| update_grow/k32 | 414.32 | 416.83 | 409.36 | 1.20% | 1.79% | 4/6 / 4/6 |
| update_grow/k8 | 398.42 | 396.01 | 399.16 | -0.19% | -0.79% | 3/6 / 3/6 |

### 原有页级合并

| 负载 | 原始基线 ns | 上一版 ns | 本版 ns | 对基线耗时下降 | 对上一版耗时下降 | 更快轮数（基线 / 上一版） |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| merge_insert/k128/m1 | 93.36 | 56.25 | 60.64 | 35.04% | -7.81% | 6/6 / 1/6 |
| merge_insert/k128/m12 | 772.46 | 484.57 | 378.81 | 50.96% | 21.83% | 6/6 / 6/6 |
| merge_insert/k128/m4 | 273.24 | 160.45 | 161.91 | 40.74% | -0.91% | 6/6 / 2/6 |
| merge_insert/k8/m1 | 109.28 | 83.30 | 84.67 | 22.52% | -1.64% | 6/6 / 2/6 |
| merge_insert/k8/m16 | 1039.45 | 946.68 | 574.80 | 44.70% | 39.28% | 6/6 / 6/6 |
| merge_insert/k8/m4 | 285.16 | 235.06 | 195.90 | 31.30% | 16.66% | 6/6 / 6/6 |
| merge_overwrite/k128/m1 | 78.32 | 46.19 | 49.71 | 36.53% | -7.61% | 6/6 / 2/6 |
| merge_overwrite/k128/m12 | 600.78 | 306.74 | 281.93 | 53.07% | 8.09% | 6/6 / 5/6 |
| merge_overwrite/k128/m4 | 216.21 | 111.82 | 108.89 | 49.64% | 2.62% | 6/6 / 6/6 |
| merge_overwrite/k8/m1 | 109.57 | 66.50 | 65.62 | 40.11% | 1.32% | 6/6 / 4/6 |
| merge_overwrite/k8/m16 | 889.45 | 732.71 | 465.23 | 47.69% | 36.51% | 6/6 / 6/6 |
| merge_overwrite/k8/m4 | 303.12 | 150.10 | 139.84 | 53.87% | 6.83% | 6/6 / 5/6 |

### 碎片页连续增长

| 负载 | 上一版 ns | 本版 ns | 耗时下降 | 更快轮数 |
| --- | ---: | ---: | ---: | --- |
| growth/k128/prefixfalse/m1 | 105.66 | 93.14 | 11.85% | 5/6 |
| growth/k128/prefixfalse/m4 | 270.97 | 227.39 | 16.08% | 6/6 |
| growth/k128/prefixtrue/m1 | 199.12 | 132.50 | 33.46% | 6/6 |
| growth/k128/prefixtrue/m4 | 298.00 | 228.30 | 23.39% | 6/6 |
| growth/k8/prefixfalse/m1 | 135.60 | 110.23 | 18.71% | 6/6 |
| growth/k8/prefixfalse/m4 | 222.90 | 189.50 | 14.98% | 6/6 |
| growth/k8/prefixtrue/m1 | 153.83 | 127.37 | 17.20% | 6/6 |
| growth/k8/prefixtrue/m4 | 238.77 | 199.73 | 16.35% | 6/6 |

### 分段诊断

另行开启 `BFTREE_BENCH_PHASES=1`，对目标负载做六轮相同顺序的对照。
扫描阶段（包含扫描触发的合并）相对上一版下降 **10.15%**；
更新阶段慢 **0.27%**，3/6 轮更快，接近持平。这支持主要收益来自归并路径。
额外计时器会扰动结果，诊断数据不与上面的正式结果混算。

| 阶段 | 原始基线 ns | 上一版 ns | 本版 ns | 单位 |
| --- | ---: | ---: | ---: | --- |
| update | 204.54 | 198.39 | 198.93 | ns/更新 |
| scan | 31.78 | 34.15 | 30.68 | ns/扫描记录 |

## 验证

| 检查 | 结果 |
| --- | --- |
| 最终 Debug 叶页测试 | 40 项通过；3 个手动基准忽略 |
| 最终完整 Release 测试 | 136 个单元测试、1 个集成测试、10 个 doctest 通过；3 个手动基准及 2 个既有 doctest 忽略 |
| 随机归并模型测试 | 256 组案例通过，覆盖不同公共前缀、覆盖/删除及 Cache/Phantom |
| 默认 Miri 别名检查 | 4 项通过：短后缀/预览/reference bit、归并成功、归并失败及 fence 预留、稀疏覆盖/增长；Windows 主机解释 Linux 目标 |
| 库及两个公共基准的 Clippy | `-D warnings` 通过 |
| 三种 runtime metrics 配置的库 Clippy | `metrics-rt`、`metrics-rt-debug-all`、`metrics-rt-debug-timer` 均通过 |
| 格式与 diff 空白检查 | 通过 |

新增边界测试覆盖空后缀、单字节后缀、二进制及 120B 公共前缀、
非零未使用 preview 字节、reference bit 重置、覆盖/删除、
fence/LSN/快照版本保持。新增增长微基准验证每次合并成功及最终全部键值。

本轮没有重跑 Shuttle。上一轮原始基线和候选均出现的两项
`RefCell already borrowed` 失败仍未解决，不能把本次测试结果解释为
Shuttle 已通过或整个并发协议已获验证。未运行 ASan、Linux 实机、
并发性能及冷盘/持久化性能测试。

## 数据及复现

- [最终三版本原始数据，702 行](algorithm-benchmark-2026-10-20-sparse.csv)
- [增长微基准原始数据，96 行](algorithm-benchmark-2026-10-20-growth.csv)
- [分段诊断原始数据，54 行](algorithm-benchmark-2026-10-20-phases.csv)
- [未采用中间候选的完整对照，702 行](algorithm-benchmark-2026-10-20-sparse-exploratory.csv)
- [源码、驱动、二进制及数据 SHA-256](algorithm-benchmark-2026-10-20-sparse-manifest.json)

探索数据使用含前缀容量预估改动的中间候选，其中目标负载有所改善，
但 cache-only 扫描及部分单条合并出现回退；它不代表最终版本。
其余筛选测量、源码快照和测试日志保存在本机 `target/sparse-20261020/`。

```powershell
# 每个目录先放置冻结的 in_memory.exe、algorithm_paths.exe、leaf_tests.exe
.\benchmark\compare-algorithms.ps1 `
  -BinaryRoot H:\git\bf-tree\target\sparse-20261020\bin `
  -OutputCsv H:\git\bf-tree\target\sparse-20261020\reproduced.csv `
  -Pairs 6 -Samples 9 -Revisions baseline,previous,candidate

# 新增长基准只比较能够成功完成相同工作的两个优化版本
.\benchmark\compare-algorithms.ps1 `
  -BinaryRoot H:\git\bf-tree\target\sparse-20261020\bin `
  -OutputCsv H:\git\bf-tree\target\sparse-20261020\reproduced-growth.csv `
  -Pairs 6 -Samples 9 -Revisions previous,candidate -Suites growth

cargo test --locked --release
cargo clippy --locked --lib --bench in_memory --bench algorithm_paths -- -D warnings
cargo fmt --all -- --check
cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_sorted_merge_preserves_short_suffixes
cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_merge_ -- --skip leaf_merge_microbench
cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_sparse_merge_accounts
```
