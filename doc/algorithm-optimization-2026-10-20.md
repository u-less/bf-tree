# 2026-10-20 算法优化与基准报告

后续针对长键稀疏更新退化的修改及最新三版本对照，见
[后续优化报告](sparse-merge-optimization-2026-10-20.md)。本文保留本轮历史结果。

本轮对照基线为 `6fd826fb1a3a4a29cfa0902428428376449b667d`。修改集中在
mini page 合并及 cache-only 页扩容前的空间回收，不改变公共 API、页布局、
WAL 格式或快照格式。双游标扫描、跨页路径复用和自适应查找不包含在本轮实现中。

## 实现

### 有序归并与精确容量判断

原来的 `merge_mini_page` 为每条 mini page 记录分配完整键，对脏记录逐条
执行普通插入；容量检查按每条 Insert 都是新记录估算。覆盖已有记录时，
这个估算可能把本来能容纳的更新送入分裂路径。

现在先按增量大小选择路径：

- 空间充足的稀疏增量仍逐条更新，直接借用无压缩前缀的键；有前缀时复用键缓冲区。
- 增量超过 4 条且不少于基础页记录数的四分之一时，使用双路有序归并。
- 稀疏增量若未通过保守容量检查，再计算原地更新实际需要的空间。可以复用旧值槽的
  覆盖更新不消耗新空间；增长更新计入新负载，新键还计入元数据。
- 原地更新仍放不下时，尝试整理后的有序归并；仅最终结果和 fence 预留空间仍放不下时
  返回失败，交给现有分裂逻辑。

归并在独立的、由 RAII 管理的临时页中构造结果，成功后才复制到基础页。
失败不会修改原页内容或快照版本。基础页 LSN 和边界保持不变，快照版本使用
mini page 的 clean version。mini page 中的 Cache/Phantom 不覆盖基础页，
Insert 覆盖旧值，Delete 屏蔽旧记录，基础页原有删除记录在重建时移除。

设基础页 n 条、增量 m 条，逐条插入新键的元数据搬移最坏可达 O(mn)；
有序归并的记录处理为 O(n+m)，另加键比较、键值复制及页镜像复制成本。
小增量仍保留原地更新，避免无条件复制整页。上述阈值是本轮实测采用的启发式，
不是适用于所有硬件和负载的最优值。

实现见 [leaf_node.rs](../src/nodes/leaf_node.rs)。

### cache-only 页面扩容前回收

非最大尺寸的 cache-only 页插入失败时，检查删除、缩短值和历史增长更新留下的空间。
只有新记录能在整理后容纳，且可回收空间至少达到页有效容量的四分之一，才整理并重试。
替换旧键时只保留新值，避免先复制旧值再增长。最大页继续使用原有整理/分裂路径。

这一策略限于 cache-only 模式，磁盘 mini page 的删除标记仍保留。
普通 cache-only 树分裂后页面通常已达到最大尺寸；本策略主要针对仍处于较小尺寸的页，
不能将下面所有 cache-only 指标的变化都归因于它。

实现见 [mini_page_op.rs](../src/mini_page_op.rs)。

## 基准方法

- Intel Core i7-13700KF，Windows 11，系统版本 `10.0.26220`。
- Rust `1.98.1 (48a229cea 2026-09-01)`，LLVM 22.1.8，MSVC x64。
- 普通 release/bench 配置，未启用 LTO、PGO 或 `target-cpu=native`。
- 两个版本使用同一 Cargo.lock，SHA-256：
  `135d5406b19be87047b2a12e960497dfc28100d4d2a66a7dca887801fc21da73`。
- 基线来自 `git archive`，仅加入相同基准驱动及其注册项；生产源码保持基线提交。
  编译后冻结两个版本的可执行文件。正式对比时停止本任务的编译、测试及 Miri。
- 进程固定到逻辑 CPU 4（affinity mask 16），不修改机器全局电源设置。
- 5 轮配对，按 B/C、C/B 交替顺序运行；每个进程丢弃一次预热，取 9 个样本。
  表中为 5 个进程中位数的中位数；耗时下降 = `100 × (1 - 候选 / 基线)`。
  原始数据保留每轮中位数、最小值和最大值，不删除不利样本。

负载：

1. 原有 `in_memory`：16,384 个键，8/32/128B 键，64 MiB cache-only 树；
   插入、命中/未命中读取、增长更新及扫描，共 15 项。
2. 新增 `algorithm_paths`：4,096 个键，8/128B 键，8 MiB 缓存；
   先插入 32B 值并扫描，再进行 4 轮更新后全扫描，值长度依次为 64/96/128/32B。
   stride 1 更新所有记录，stride 16 按固定打乱顺序每 16 条更新一条。
   每轮使用新树，初始化及最后的逐键、逐记录核验在计时区外。
   单位为一次更新或一条扫描返回记录的平均 ns，不能解读为纯更新延迟。
3. 页级微基准：4 KiB 基础页，8B 键 64 条、128B 键 12 条，16B 值；
   每个样本对 512 个预先构造的独立页面各合并一次，覆盖新键插入和同长度覆盖更新，
   并区分 1/4/16 条（128B 键最多 12 条）增量。单位为 ns/次整页合并。
   所有页面的结果在计时后检查。

`paged_memory` 使用内存后端执行磁盘页算法；`file_buffered` 使用普通文件及操作系统
页缓存。后者未测 fsync、冷盘、断电持久性或 SSD 极限吞吐。所有性能测量均为单线程；
不外推并发伸缩或生产 p99。

## 测量结果

文件后端的四项更新＋扫描负载耗时下降 **22.49%～35.21%**，每项均有 5/5 对更快。页级合并的 12 项负载下降 **6.17%～55.59%**，同样均为 5/5 对更快。

普通 cache-only 的 15 项差异在 -1.51%～+0.68% 之间，多数配对方向不一致，不主张获得确定的通用吞吐提升。较小页面回收策略有针对性功能验证，本轮没有分离测出它的独立性能收益。

仍有不利结果：`update_scan/paged_memory/k128/stride16` 从 40.15 ns 上升到 42.29 ns，慢 **5.32%**，5/5 对均更慢；配对百分比的中位数为慢 3.54%。应把它视为当前实现的已测退化，不能以文件后端收益覆盖掉。减少保守分裂和重建会改变后续页面形态与 CPU 工作量，具体归因仍需进一步分析。

### 更新与扫描的组合负载

| 负载 | 基线 ns | 候选 ns | 耗时下降 | 更快配对数 |
| --- | ---: | ---: | ---: | ---: |
| update_scan/cache/k8/stride1 | 163.38 | 164.44 | -0.65% | 2/5 |
| update_scan/cache/k8/stride16 | 33.04 | 33.24 | -0.62% | 1/5 |
| update_scan/paged_memory/k8/stride1 | 211.66 | 177.18 | 16.29% | 5/5 |
| update_scan/paged_memory/k8/stride16 | 41.96 | 32.93 | 21.53% | 5/5 |
| update_scan/file_buffered/k8/stride1 | 453.01 | 334.59 | 26.14% | 5/5 |
| update_scan/file_buffered/k8/stride16 | 133.97 | 97.62 | 27.13% | 5/5 |
| update_scan/cache/k128/stride1 | 192.75 | 193.24 | -0.25% | 4/5 |
| update_scan/cache/k128/stride16 | 43.49 | 42.89 | 1.36% | 3/5 |
| update_scan/paged_memory/k128/stride1 | 278.86 | 225.31 | 19.20% | 5/5 |
| update_scan/paged_memory/k128/stride16 | 40.15 | 42.29 | -5.32% | 0/5 |
| update_scan/file_buffered/k128/stride1 | 766.81 | 496.78 | 35.21% | 5/5 |
| update_scan/file_buffered/k128/stride16 | 142.91 | 110.77 | 22.49% | 5/5 |

### 原有 cache-only 完整树负载

扫描单位为 ns/返回记录；其他项目为 ns/操作。

| 负载 | 基线 ns | 候选 ns | 耗时下降 | 更快配对数 |
| --- | ---: | ---: | ---: | ---: |
| insert/k8 | 274.02 | 278.16 | -1.51% | 2/5 |
| read_hit/k8 | 171.48 | 171.63 | -0.09% | 3/5 |
| read_miss/k8 | 180.34 | 179.10 | 0.68% | 3/5 |
| update_grow/k8 | 392.28 | 390.95 | 0.34% | 1/5 |
| scan/k8 | 12.56 | 12.50 | 0.49% | 3/5 |
| insert/k32 | 313.06 | 314.70 | -0.52% | 3/5 |
| read_hit/k32 | 195.13 | 196.95 | -0.93% | 1/5 |
| read_miss/k32 | 205.57 | 206.79 | -0.60% | 2/5 |
| update_grow/k32 | 408.25 | 406.52 | 0.42% | 3/5 |
| scan/k32 | 15.06 | 14.97 | 0.64% | 3/5 |
| insert/k128 | 420.90 | 420.03 | 0.21% | 3/5 |
| read_hit/k128 | 257.09 | 258.22 | -0.44% | 2/5 |
| read_miss/k128 | 267.48 | 267.54 | -0.02% | 4/5 |
| update_grow/k128 | 475.45 | 475.65 | -0.04% | 1/5 |
| scan/k128 | 27.84 | 27.75 | 0.32% | 4/5 |

### 页级合并

m 表示每次合并的增量记录数。这里的覆盖更新是同长度覆盖；单页计时包含归并缓冲区的申请与释放，但不包含输入页面构造。

| 负载 | 基线 ns | 候选 ns | 耗时下降 | 更快配对数 |
| --- | ---: | ---: | ---: | ---: |
| merge_insert/k8/m1 | 108.98 | 80.66 | 25.99% | 5/5 |
| merge_overwrite/k8/m1 | 108.40 | 60.94 | 43.78% | 5/5 |
| merge_insert/k8/m4 | 284.57 | 183.59 | 35.48% | 5/5 |
| merge_overwrite/k8/m4 | 298.63 | 132.62 | 55.59% | 5/5 |
| merge_insert/k8/m16 | 1013.67 | 951.17 | 6.17% | 5/5 |
| merge_overwrite/k8/m16 | 877.73 | 740.43 | 15.64% | 5/5 |
| merge_insert/k128/m1 | 88.87 | 50.59 | 43.08% | 5/5 |
| merge_overwrite/k128/m1 | 72.46 | 44.92 | 38.01% | 5/5 |
| merge_insert/k128/m4 | 271.09 | 157.03 | 42.08% | 5/5 |
| merge_overwrite/k128/m4 | 206.25 | 104.88 | 49.15% | 5/5 |
| merge_insert/k128/m12 | 770.51 | 493.36 | 35.97% | 5/5 |
| merge_overwrite/k128/m12 | 595.70 | 300.59 | 49.54% | 5/5 |

正式 [390 行原始数据](algorithm-benchmark-2026-10-20.csv) 与 [源码及可执行文件 SHA-256](algorithm-benchmark-2026-10-20-manifest.json) 随报告保存。

实现迭代时发现长键稀疏负载退化，随后补上实际新增字节判断。此前同配置的五轮 [探索性数据](algorithm-benchmark-2026-10-20-exploratory.csv) 也保留，其中该负载慢 10.05%。探索性与最终结果分别使用各自的基线配对，不混合计算；两次实验的硬件噪声也不同，不能把 10.05%→5.32% 全部当作单项改动的因果收益。

## 验证

| 检查 | 结果 |
| --- | --- |
| 初版完整 Debug 测试 | 134 个单元测试、1 个集成测试、10 个 doctest 通过；2 个手动基准和 2 个既有 doctest 忽略 |
| 最终 Debug 叶页相关测试 | 39 项通过，2 个手动基准忽略 |
| 最终完整 Release 测试 | 135 个单元测试、1 个集成测试、10 个 doctest 通过；忽略项同上 |
| 归并模型测试 | 256 组随机案例通过，包含长/二进制前缀、两种 mini page 前缀形式、覆盖、删除、Cache/Phantom |
| 默认 Miri 别名检查 | 合并成功/失败、cache-only 空间回收、稀疏覆盖/增长共 4 项通过；Windows 主机解释 Linux 目标 |
| 库与两个公共基准的 Clippy | `-D warnings` 通过 |
| 三种 runtime metrics 配置的库 Clippy | `metrics-rt`、`metrics-rt-debug-all`、`metrics-rt-debug-timer` 全部通过 |
| 格式与 diff 空白检查 | 通过 |
| Shuttle cache-only 快照 | 通过原有测试规模 |
| Shuttle 核心操作、磁盘快照 | 未通过；基线也复现相同内部错误，见下文 |
| 包含历史测试代码的全目标 Clippy | 未通过；有 34 项既有测试告警，未将这些无关清理纳入本轮 |

新增 5 个普通测试及 1 个手动微基准。重点覆盖：覆盖更新能在原页容纳、失败时
页镜像不变、保留 fence 空间、LSN/版本/边界保持、稀疏覆盖不额外占用空间、
增长更新回收旧负载、空间回收失败不改变原页。最终 Release 还运行原有并发压力、
快照恢复和磁盘/内存混合操作测试。

Shuttle 的核心操作及磁盘快照测试在候选与基线中均报告
`shuttle-0.7.1/src/sync/atomic/mod.rs:215:20: RefCell already borrowed`。
磁盘快照单独运行也复现；一次开启 backtrace 的候选诊断还在失败处理期间
以 `STATUS_ACCESS_VIOLATION` 退出。本报告不将这些检查记为通过，
也不据此认定整个引擎的并发协议安全。两边的日志均保留在本轮 `target/` 目录。
这些 Shuttle 检查发生在最后一次稀疏容量判断细化之前；最终代码重新通过完整
Release 测试，但未重跑全部 Shuttle 调度。

本轮没有修改此前报告中列出的内部节点乐观读等并发协议限制，也未执行 ASan、
Linux 实机性能、故障注入或生产尾延迟验证。Miri 的通过仅覆盖所执行的页面路径。

## 复现

```powershell
cargo test --locked
cargo test --locked --release
cargo clippy --locked --lib -- -D warnings
cargo clippy --locked --bench in_memory --bench algorithm_paths -- -D warnings
cargo fmt --all -- --check

cargo bench --locked --bench in_memory
cargo bench --locked --bench algorithm_paths
cargo test --locked --release --lib leaf_merge_microbench -- --ignored --nocapture --test-threads=1

cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_merge_ -- --skip leaf_merge_microbench
cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_cache_reclaim_keeps_capacity
cargo +nightly-2026-09-07 miri test --locked --target x86_64-unknown-linux-gnu --lib leaf_sparse_merge_accounts
```

对照测量需将本轮 `benches/algorithm_paths.rs`、`src/tests/algorithm_bench.rs`
原样复制到基线，并分别注册 Cargo bench 及测试模块；原有 `in_memory.rs` 不变。
基线和候选应使用独立构建目录，随后将三个可执行文件冻结为
`baseline/{in_memory,algorithm_paths,leaf_tests}.exe` 和对应的 `candidate/` 文件。
不要用共享 Cargo target 目录中的缓存产物充当两个不同版本。

```powershell
.\benchmark\compare-algorithms.ps1 `
  -BinaryRoot H:\git\bf-tree\target\algorithm-20261020\bin `
  -OutputCsv H:\git\bf-tree\target\algorithm-20261020\reproduced.csv `
  -Pairs 5 -Samples 9 -Affinity 16
```

本机冻结的二进制、构建日志、测试日志与探索性测量保存在
`target/algorithm-20261020/`。正式数据及可执行文件身份记录随本报告保存在 `doc/`。

