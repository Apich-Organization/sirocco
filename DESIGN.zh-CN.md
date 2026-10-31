<p align="center">
  <img src="logo.svg" alt="Sirocco" width="90">
</p>

<h1 align="center">Sirocco — 设计文档</h1>

<p align="center">
  <a href="README.md">README</a> ·
  <a href="README.zh-CN.md">README（中文）</a> ·
  <a href="DESIGN.md">Design</a> ·
  <b>设计文档</b>
</p>

---

本文描述的是**已经建成的系统**，并为每一个不显然的决定给出理由。它不是路线图：
写在这里的东西都存在，不存在的写在[已知限制](#17-已知限制)里，而不是排进计划。

贯穿全文的主张是：拷贝引擎的职责，是在两台设备之间搬字节时，把**干扰**降到最小——
页缓存的干扰、运行时的干扰、以及一个靠猜的调度器的干扰。下面每一条都是这个主张的推论。

## 1. 范围与立场

| 项 | 值 |
|---|---|
| 名称 | **Sirocco** —— 地中海的一种风 |
| 二进制 | `sirocco`（命令行）、`sirocco-gui`（桌面） |
| 版本 | 见 `Cargo.toml` 的 `[workspace.package]` |
| 语言 | Rust，edition 2024；下限见 `Cargo.toml` 的 `rust-version` |
| 依赖 | Windows：**无** —— 手写 `kernel32` / `advapi32` / `ntdll` 绑定。Linux：仅 `libc` |
| 明确不用 | Tokio、async-std、windows-sys、clap、liburing、libacl |

"不依赖"不是洁癖。IO 引擎需要精确控制每一个 `OVERLAPPED` 的生命周期、控制复用哪个
SQE 槽位、控制缓冲区注册、控制亲和性与 I/O 优先级。任何一层封装要么把这些藏起来，
要么在关键路径上多一次分配。这不是说运行时不可能快，而是说引擎的全部价值就在于
运行时不允许你做的那些决定。

Sirocco 不是通用文件系统 API。它是一个只做一件事的程序，设计允许建立在这个前提上。

## 2. 总体架构

```
┌──────────────────────────────────────────────────────────────────────┐
│ sirocco-cli        参数解析 / 报告 / bench / info / verify            │
│ sirocco-gui        同一引擎之上的 iced 前端                            │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-engine     会话编排、扫描、决策、流水线、日志                  │
│   ├─ scan      并行目录遍历（work-stealing）                          │
│   ├─ session   跳过 / 全量 / 追加 / 增量 决策 + 调度 + 汇总            │
│   ├─ pipeline  reader×R / writer×W，各自持有 reactor                  │
│   ├─ journal   断点续传                                              │
│   └─ config    全部旋钮及其归一化                                     │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-core       与 OS 无关的算法                                   │
│   ├─ delta     成本模型 + Wilson 上界                                 │
│   ├─ entropy   实测可压缩性，不是估计                                  │
│   ├─ lz        LZ4 block 编解码（自研，wire 兼容）                     │
│   ├─ hash      XXH64 / CRC32C                                        │
│   ├─ queue     有界无锁 MPMC（Vyukov）                                │
│   ├─ plan      作业描述类型                                          │
│   └─ pattern   include / exclude 通配符匹配                           │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-sys        平台层：只暴露 RawFile / Reactor / Topology         │
│   ├─ win/       CreateFileW + IOCP + FSCTL + SetFileValidData         │
│   ├─ lnx/       open(O_DIRECT) + 裸 io_uring + FIEMAP + fallocate     │
│   └─ blocking   可移植回退（每线程独立句柄）                            │
└──────────────────────────────────────────────────────────────────────┘
```

**承重约束**：一个 `Reactor` 由**恰好一个线程**独占。

Windows 上一个文件句柄只能关联一个完成端口，所以"每线程一套句柄"不是优化而是必然；
Linux 上每个线程一个 ring，形状相同。这条约束在两个平台都成立，因此上层代码只写一遍。

它派生出两条不变量，而不是两条建议：

1. 句柄缓存键是 `(文件 id << 3) | kind`，`kind` 必须区分 direct 与 buffered。
   不区分的缓存会把一个 `NO_BUFFERING` 句柄交给非对齐的尾部读，Windows 上
   `ERROR_INVALID_PARAMETER`，Linux 上 `EINVAL`。
2. 关闭信号必须在每个队列都满的时候仍然投递得出去。每一处阻塞 push 都有 abort 逃逸；
   没有逃逸，一次中断就会把 join 挂住。

## 3. 平台层

### 3.1 Windows

| 能力 | 机制 |
|---|---|
| 直接 I/O | `FILE_FLAG_NO_BUFFERING` + 4 KiB 对齐的池化缓冲 |
| 异步 | `FILE_FLAG_OVERLAPPED` + `CreateIoCompletionPort` + `GetQueuedCompletionStatusEx`（批量收割） |
| 完成记录 | `OVERLAPPED` 放在预分配的连续 slab 里，指针算术反查下标，提交零分配 |
| 错误翻译 | `OVERLAPPED.Internal` 是 NTSTATUS，用 `GetOverlappedResult` 转成 Win32 错误码 |
| 预分配 | `SetFileValidData`（需 `SeManageVolumePrivilege`），否则走稀疏空洞（§9） |
| 稀疏 | `FSCTL_SET_SPARSE`（可置位可清除）、`FSCTL_SET_ZERO_DATA` 打洞 |
| 物理布局 | `FSCTL_GET_RETRIEVAL_POINTERS`（VCN→LCN）；MFT 序号取 `FileIdInfo` 的低 64 位 |
| 介质类型 | `IOCTL_STORAGE_QUERY_PROPERTY`：`StorageDeviceSeekPenaltyProperty`、`StorageAccessAlignmentProperty` |
| 压缩 | `FSCTL_SET_COMPRESSION`（NTFS 透明压缩） |
| 拓扑 | `GetLogicalProcessorInformationEx(RelationAll)`，解析 `PROCESSOR_RELATIONSHIP` / `NUMA_NODE_RELATIONSHIP` |
| 亲和性 | `SetThreadGroupAffinity` —— 处理器组感知，>64 逻辑核也正确而不是被截断 |
| I/O 优先级 | `SetFileInformationByHandle(FileIoPriorityHintInfo)` —— **按句柄**，不是按线程 |
| 进程 / 线程优先级 | `SetPriorityClass` / `SetThreadPriority` |
| 大页 | `VirtualAlloc(MEM_LARGE_PAGES)`，需 `SeLockMemoryPrivilege` |
| 特权 | `OpenProcessToken` + `AdjustTokenPrivileges` |

最后一行是个值得点名的坑：`AdjustTokenPrivileges` 即使什么都没改也返回成功。唯一可靠的
判断是之后再看 `GetLastError` 是否等于 `ERROR_NOT_ALL_ASSIGNED`。相信返回值的探测会
报告进程并不具备的能力，而失败要到很久以后才以"某条路径莫名其妙变慢"的形式出现。

### 3.2 Linux

| 能力 | 机制 |
|---|---|
| 直接 I/O | `open(O_DIRECT)` |
| 异步 | 手写 `io_uring`：`io_uring_setup` / `enter` / `register` 三个裸系统调用，自己 mmap SQ/CQ 环 |
| 优化标志 | `IORING_SETUP_COOP_TASKRUN \| SINGLE_ISSUER`，可选 `IOPOLL` / `SQPOLL` |
| 固定缓冲 | `IORING_REGISTER_BUFFERS` + `READ_FIXED` / `WRITE_FIXED`，把 `get_user_pages` 从每个请求里去掉 |
| 预分配 | `fallocate`；打洞是 `fallocate(PUNCH_HOLE \| KEEP_SIZE)` |
| 物理布局 | `FS_IOC_FIEMAP` |
| 介质类型 | `/sys/block/<dev>/queue/rotational` |
| 拓扑 | `/sys/devices/system/cpu/cpuN/topology/*` 加 `sched_getaffinity` |
| 亲和性 | `sched_setaffinity` |
| I/O 优先级 | `ioprio_set`，best-effort 类，0..4 映射到 data 7..0 |
| 大页 | `mmap(MAP_HUGETLB)`，失败回退 `madvise(MADV_HUGEPAGE)` |
| 缓存控制 | `posix_fadvise(SEQUENTIAL / DONTNEED)` |
| 特权 | 只读 `/proc/self/status` 的 `CapEff` —— Linux 的能力在 exec 时授予，运行时无法提权 |

两条可移植性决定不显然，所以写下来：

- **系统调用号一律取自 `libc::SYS_*`，不写字面量。** 它们逐架构不同：`ioprio_set` 在
  x86_64 上是 251、在 aarch64 上是 30，`statx` 是 332 与 291，`mbind` 是 237 与 235。
  硬编码的数字不会报错，它只是调用另一个系统调用。
- **`io_uring` 的结构体布局在编译期校验**（对 `size_of` 和字段偏移做 `const` 断言）。
  将来内核改了 ABI，构建失败，而不是环悄悄地行为错乱。

### 3.3 可移植回退

`--engine=blocking` 是 `pread`/`pwrite` 上的线程池。它存在两个理由：它是快速路径的
交叉验证基准；以及当 native 后端不可用时它就是兜底（内核用 sysctl 或 seccomp 关掉了
`io_uring`，或者 Windows 版本缺少快速路径依赖的完成端口 API）。

它还带来了这个项目里最清楚的一条平台教训。第一版让多个工作线程共享一个 `RawFile`，
结果在测试用的云盘上**并发 `ReadFile` 会随机返回 0 字节** —— `OVERLAPPED` 里的偏移被
忽略，调用退化成共享文件指针。改成每个工作线程独立打开句柄后完全正常。IOCP 后端本来
就是每线程独立句柄，所以修正只落在回退后端上；但它确立的规则（"句柄属于读它的那个
线程"）现在处处成立。

## 4. 数据通路

```
 scheduler ──Chunk──▶ read queue ──▶ [reader × R] ──Ready──▶ write queue ──▶ [writer × W] ──▶ device
    │                                     │                                        │
    │                                  buffer pool ◀───────────────────────────────┘
    │                                     │
    └─ prepare_dst ─ 创建 / 预分配 / 压缩属性 ─┘
```

一个 **chunk** 是 `(FileCtx, src_off, rlen, dst_off, wlen, len, tail)`。一个 1 MiB 的
chunk 就是一次读加一次写，不会再切碎——除非 `--io-unit` 要求更小的请求。

reader 只碰源句柄，writer 只碰目标句柄。开 `--pin` 时两组线程被钉到**不相交的物理核**
上，因此一个 reader 和一个 writer 永远不会共用同一个 SMT 兄弟核的执行单元。

缓冲池是一次大的 `VirtualAlloc` / `mmap`（可选 2 MiB 大页），切成固定大小的槽位，从
原子空闲表分配。因为底层分配是页对齐的、槽位是它的 2 的幂等分，每个缓冲区地址**天然**
满足直接 I/O 对齐——不存在需要写对的逐请求对齐算术。

背压是结构性的：两个队列都是有界无锁 MPMC（Vyukov）环。队列满时 yield 而不是睡眠，
所以不丢数据、不无界增长，一个卡住的 writer 也不会变成无界内存。

### 4.1 尾部，以及为什么对齐是一个状态机

直接 I/O 要求偏移、长度**和**缓冲区地址都按扇区对齐，而文件长度几乎从不恰好是 4 KiB
的整数倍。处理这段的步骤是引擎里最容易被写错的地方，所以它是固定的：

1. 目标文件预分配，向上取整到 4 KiB。
2. 数据块按 4 KiB 向上取整写出，缓冲区尾部补零。
3. **最后一个块用另一个非直接 I/O 句柄读** —— 直接 I/O 读越界会失败 —— 然后清零缓冲区尾部。
4. 全部写完后截断到精确长度，并恢复 mtime。

第 3 步就是句柄缓存键必须带 `kind` 的原因。如果一次缓冲尾部读命中了缓存的直接句柄，
结果是立刻 `EINVAL`，而人的第一反应是把它当成瞬时设备错误去重试——它不是。

## 5. 决定要拷什么

这是整个项目里唯一真正算法性的部分，也是问题描述里留白的那部分：**在不充分的信息下
判断增量拷贝是否值得**。

### 5.1 三种策略与成本

设文件大小 `S`，差异块比例 `r`（未知），源读 / 目标写 / 目标读吞吐为 `R`、`W`、`Rd`：

| 策略 | 时间 |
|---|---|
| 全量覆盖 | `S/R + S/W` |
| 块级增量（无索引） | `S/R + S/Rd + r·S/W` |
| 追加（前缀相同） | `(S−D)/R + (S−D)/W` |

块级增量优于全量，当且仅当

```
S/R + S/Rd + r·S/W  <  S/R + S/W
      ⟺  r < 1 − W/Rd
```

这个不等式就是全部论证。**在快盘上，增量同步通常是个坏主意**：NVMe 上 `W ≈ Rd`，
阈值退化为 0，没有任何 `r` 能满足它。只有当目标比源路径慢得多时它才划算 —— 网络盘、
USB、SMR、任何对写放大敏感的介质。工具不该猜自己处在哪种情况，所以 Sirocco **测量**
两边的速率，把数字代进不等式，让不等式做决定。

### 5.2 从过少的样本里估计 `r`

`r` 未知，那就抽样：取 `k` 个块对齐的探针（确定性伪随机，加首尾各一），两边都读，
统计不同的个数 `c`。

点估计 `c/k` 不够。一千个块里改一个，64 次抽样大概率一次都命中不了。所以决策用的是
**Wilson 置信区间 95% 上界**：

```
p̂ = c/k,  z = 1.96
upper = ( p̂ + z²/2k + z·√(p̂(1−p̂)/k + z²/4k²) ) / (1 + z²/k)
```

用上界而不是点估计，是刻意的不对称代价：把增量误判成全量，代价是多读一遍目标；把全量
误判成增量，代价是白写一遍全部数据。上界把判断推向便宜的那一侧错误。

### 5.3 子块探针的修正

探针粒度 64 KiB，比较粒度 1 MiB。一个 1 MiB 块只要有**任何**子块变了就算变，所以

```
r_block = 1 − (1 − r_probe)^(block/probe_unit)
```

指数 `inflate = 16` 就是这个比值。没有这一步，估计会系统性偏低，而"每 1 MiB 改 4 KiB"
这种常见场景会被看成 0.4% 的差异——实际是 100%。

### 5.4 追加才是真正的赢家

当目标比源短，且抽样显示公共前缀未变时，拷贝变成追加：只读尾部、只写尾部，**完全不碰
前缀**。对日志、VM 镜像、只追加的数据集，"第二次同步几乎免费"就来自这里。

追加不是盲信。提交之前，目标最后 4 MiB 会与源**逐字节**比较，任何差异都退回全量。

### 5.5 决策表

| 条件 | 动作 |
|---|---|
| 目标不存在 | 全量 |
| 大小相同且 mtime 精确相同（或双方时间戳亚秒部分都为 0 且相差 ≤ 2 s，用于 FAT/SMB） | 跳过 |
| `size < min_size`（默认 8 MiB） | 全量 —— 探针的开销超过它能省下的 |
| 目标更小、前缀抽样干净、最后 4 MiB 一致 | **追加** |
| `r_upper < (1 − W/Rd) − margin` | **块级增量** |
| 其它 | 全量 |

时间戳那条规则值得单独说明。rsync 与 robocopy 无条件给 2 秒容差；在"改完立刻同步"的
工作流里，这会静默漏掉文件。Sirocco 先精确比较，只有当双方都报告亚秒部分为 0 —— 那是
粗粒度文件系统的特征，而不是真的相等 —— 才启用容差。端到端测试抓到了最初的行为，所以
这条规则被明确写进文档。

## 6. 小文件

小文件的瓶颈不是带宽，是**每文件延迟**：一次 open、read、write、close 在好的盘上也要
几十微秒，几十万个串起来就是几十秒。

Sirocco 在队列层解决它，而不是打包成流。低于 `--small` 阈值的文件成批进入流水线，
一整批的读请求和写请求同时在飞，设备队列立刻被填满。每线程 96 个句柄的 LRU 让一个
目录下的小文件几乎不重复 open。

"把多个文件打包成一条流再拆包"的方案被否决：对普通文件拷贝，它需要第二遍把数据拆回
各自文件，总 I/O 翻倍。只有当"束"本身是交付物时才值得，比如归档或容器格式。

## 7. 压缩

在本地盘上，"先压缩再写"通常是负优化：省下的是写入量，代价是 CPU 时间，而字节最终
还是要写出去（除非文件系统自己压缩）。所以这个决定由测量做出，而不是由熵做出：

1. 抽三段共 768 KiB 样本（首 / 中 / 尾）。
2. **真的跑一遍 LZ4**，得到实测压缩比和实测压缩吞吐 `C`。熵只作为上下文记录：
   JPEG 和编译产物都在 8 bit/byte 附近，而后者仍然能压 2:1。
3. 用同一个成本模型判断：
   ```
   ratio·S/W + S/C  <  S/W   ⟺   ratio < 1 − W/C
   ```
   3 GB/s 的 NVMe 配 600 MB/s 的 LZ4 永远不成立，于是压缩自动关闭。150 MB/s 的 HDD
   上 `ratio < 0.75` 立刻成立，于是自动打开。
4. 判定为划算时，给目标文件打上 **NTFS 透明压缩**属性，由内核完成压缩。用户态不会
   在写出的路上再压一遍。

LZ4 block 编解码器是自研的（wire 兼容，几百行）。它存在的理由是第 2 步需要真实测量，
以及将来做容器或网络格式时可能要复用。它带有 round-trip 测试：随机、全零、文本、
重叠匹配、损坏输入。

## 8. 校验与索引

| 模式 | 做法 | 成本 |
|---|---|---|
| `none` | 不校验 | 0 |
| `size` | 比较最终大小 | 一次 stat |
| `size-mtime` | 大小加 mtime | 一次 stat |
| `hash` | reader 算源块哈希，writer 写完回读目标块并比较 | 多读一遍目标 |
| `sample` | 首 / 中 / 尾块 | 可忽略 |

默认摘要是 XXH64，约 13 GB/s，远高于任何存储设备；CPU 带指令时可选 CRC32C。校验在
writer 线程内完成，因此天然并行，并且能指出**具体哪个块**失败，而不只是哪个文件。

持久化索引（`--index=trust|verify`）按目标的 `(size, mtime)` 记录目标的块哈希。键的
选择本身就是设计：索引回答的是"这个目标文件自我上次写入后变过吗"，所以**源**侧的变化
与它的有效性无关，重同步因此可以完全不读目标。`verify` 额外重读源并比较哈希，一旦
发现索引过期就退回全量。

## 9. 预分配，以及一个必须写下来的坑

直觉说"先把文件扩到最终大小"能减少碎片。在 NTFS 上它同时把 valid data length 置零，
于是任何跳着写的操作都会触发**从空洞起点开始的零填充** —— 相当于把数据写了两遍。
128 MiB 拷贝的实测：

| 配置 | 吞吐 |
|---|---|
| 用 `SetEndOfFile` 预分配（无特权） | 67.8 MiB/s |
| 不预分配 | 158 MiB/s |
| **预分配走稀疏空洞** | **159 MiB/s** |

所以策略是：

1. 先试 `SetFileValidData`，持有 `SeManageVolumePrivilege` 时它是零成本的。
2. 否则把文件标成稀疏再扩展 —— 扩展变成空洞，不产生零填充 —— 写完后再**清掉稀疏标志**
   （`FSCTL_SET_SPARSE` 传 `SetSparse=FALSE`），使输出与普通文件在属性上无从区分。

特权探测每个会话做一次，在目标目录里用一个临时文件试，而不是每个文件试一次。逐文件
探测意味着每个文件都要先扩展、再发现失败、再回滚。

`--prealloc=auto|valid|sparse|off` 可以覆盖这个选择。

## 10. 调度、亲和性与 NUMA

- reader 默认取物理核的一半，writer 取其余；`--threads` 覆盖这个切分。
- 开 `--pin` 时，reader 与 writer 被钉到**不相交**的物理核 —— Windows 用处理器组掩码，
  Linux 用 `sched_setaffinity`。目的是两半线程永不共享 SMT 兄弟核。
- `--io-priority` 在 Windows 上是按句柄设置的（`FileIoPriorityHintInfo`），在 Linux 上
  是 `ioprio_set` 的 best-effort 类。它刻意不是线程优先级：提示属于请求本身，一个向
  两块设备发请求的线程应该能区别对待它们。
- `--process-priority` 与 `--thread-priority` 控制进程类与线程级别；Linux 上映射到 nice 值。
- NUMA：拓扑记录每个逻辑核所属的 node。请求了 node 时，缓冲用 `mbind` 放置，线程用
  `set_mempolicy` 放置，并报告最终选择的 node。单节点机器上这是无操作，所以默认
  `auto` 是安全的。

feeder 线程 —— 遍历计划并推送 chunk 的那些 —— 是唯一没有静态答案的调度决定，因此它是
自适应的（§14.3）。

## 11. 物理布局

机械盘上顺序访问与随机访问差两个数量级。Sirocco 在两个层面处理。

**文件级。** `--order=physical` 按物理键排序计划：Windows 用 NTFS 的 MFT 记录号
（`FileIdInfo` 低 64 位），Linux 用 inode 号。这近似于数据在盘片上的落点顺序。代价是
每个文件多一次 open，所以是 opt-in。

**块级。** `physical_extents()` 通过 `FSCTL_GET_RETRIEVAL_POINTERS` 或 `FIEMAP` 拿到
VCN→LCN 映射。把一个文件的 chunk 按 LCN 排序，它的读请求就按磁盘顺序发出。空洞排在
最后，因为那里没有东西可寻道。

两者都由介质类型决定（`IncursSeekPenalty`、`queue/rotational`）。在 SSD 上物理排序是
纯开销，因此默认关闭。

## 12. 权限分级与能力探测

每一个有价值的系统特性都挂在权限上。与其让用户记一堆 flag，不如启动时**实测**一遍，
再按结果分叉。

| 能力 | Windows 依据 | Linux 依据 | 解锁的路径 |
|---|---|---|---|
| `instant_alloc` | `SeManageVolumePrivilege` | `fallocate`（无需特权） | `SetFileValidData`，跳过零填充 |
| `large_pages` | `SeLockMemoryPrivilege` | `CAP_IPC_LOCK` | 2 MiB 页缓冲池 |
| `backup` | `SeBackupPrivilege` | `CAP_DAC_READ_SEARCH` | 打开 DACL 拒绝的文件 |
| `restore` | `SeRestorePrivilege` | `CAP_FOWNER` | 恢复属主与权限 |
| `rt_io` | — | `CAP_SYS_ADMIN` | `IOPRIO_CLASS_RT` |
| `rt_priority` | `SeIncreaseBasePriorityPrivilege` | `CAP_SYS_NICE` | 实时线程优先级 |
| `mlock` | 工作集配额 | `CAP_IPC_LOCK` | 锁定缓冲池页 |
| `noatime` | — | 文件属主 | `O_NOATIME` |

引擎启动时打印一行，例如：

```
sirocco: capabilities: unprivileged [large-pages, mlock]
sirocco: allocation strategy: hole-based extension
```

这就是"不同权限下走不同路径"的具体形态：同一个二进制，普通用户下走空洞扩展，一旦拿到
特权就自动改用 `SetFileValidData`。`--privileged` 会在探测之前显式申请特权；不加它，
进程就用手上已有的令牌。

## 13. 元数据保真

**硬链接。** 扫描阶段记录文件 id 与链接数 —— Linux 上 `stat` 免费给出，Windows 上用
`NtQueryInformationFile(FileStandardInformation)` 加 `FileIdInfo`。第一个路径正常拷贝；
其余同 id 的路径直接 `CreateHardLinkW` / `link(2)`，不再搬数据。`--links` 默认开启。
兄弟文件可能同时在飞，所以链接失败会短暂重试，仍失败则退回复制；正确性从不依赖赢得
这个竞争。

**时间与权限位。** Windows 用一次 `SetFileInformationByHandle(FileBasicInfo)` 写全属性
与 mtime。Linux 通过句柄写权限位（`fchmod`），因此**可执行位得以保留** —— 这在早期版本
里是真实缺口，现在由 `permissions_are_preserved` 锁住。

**扩展属性与 ACL。** `--xattrs` 通过 `fgetxattr` / `fsetxattr` 逐个复制。POSIX ACL 就是
扩展属性（`system.posix_acl_access`、`system.posix_acl_default`），所以复用这条通道，
不引入 libacl。Windows 上 `--acl` 用 `GetSecurityInfo` / `SetSecurityInfo` 复制属主、
组和 DACL；设置属主需要 `SeRestorePrivilege`，没有时降级为只写 DACL。

ACL 在两个平台上都是 opt-in。跨文件系统复制安全描述符失败得足够频繁 —— SID 无法映射、
ACE 类型不支持 —— 所以它应该是一个决定，而不是默认。

## 14. 同时做不止一件事

### 14.1 稀疏源根本不读

`--sparse` 有两层。源侧用 `FSCTL_QUERY_ALLOCATED_RANGES` 或 `SEEK_DATA`/`SEEK_HOLE`
拿到已分配区间，整块落在未分配区内的 chunk **根本不读**：writer 收到一条 `Hole` 消息，
改为在目标上打洞。这样的 chunk 连缓冲区都不占。目标侧，剩下的 chunk 若内容全零，仍按
老办法打洞。

实测一个 64 MiB、只分配了 3 MiB 的文件：

```
data : 3.00 MiB written, 3.00 MiB read, 0 B verified, 63.9 MiB punched
```

有一个后果容易被忽略：打洞之后**不能**再清除目标的稀疏标志 —— 那会把洞实体化成零簇，
把整件事白做一遍。预分配路径要清这个标志（§9），稀疏路径不能清。

### 14.2 跨进程切分

`--procs=N` 不在每个文件上拆进程 —— 那只会让多个进程抢同一个目录锁。切分发生在
**子树粒度**：父进程扫描，按字节数把遍历区间切成 N 段连续的区间，先一次性建好所有目录，
再给每个 worker 一份清单（`--files-from`）独立运行。

```
$ sirocco copy src dst --procs=4
sirocco: split into 4 processes (20000 files)
sirocco split summary (4 processes)
  files        : 20000 copied, 0 failed
  elapsed      : 7s
```

同一棵 20 000 个文件的树：单进程 14.5 s，4 进程 **7 s**。收益来自绕开目录锁而不是更多
线程 —— `robocopy /MT:16` 在同一棵树上要 10.5 s。

### 14.3 没有通道的跨进程进度

每一个直觉方案都有代价：管道每条更新一次 syscall，socket 要一个线程，共享内存加发布
线程仍然要一个线程。而这条路径每秒要跑几十万次。

零成本的做法是**让计数器本身住在共享内存里**。`Counters` 是 `#[repr(C)]` 的
`AtomicU64` 结构体；子进程把 `Stats` 直接建在映射区上，于是流水线原本就在做的
`fetch_add` 落在父进程能读的页里。

- 父进程为每个 worker 建一个映射文件（`<tmp>/stats-<i>.shm`），通过 `--shm=<path>` 传给子进程。
- 子进程调用 `Stats::from_shared(ptr)`，此后所有计数天然可见。
- 父进程每秒读四次这些缓存行并求和。

没有额外线程，没有 IPC，热路径上没有 syscall。结构体首位放一个 `magic` 字段，让陈旧或
不相干的映射被拒绝而不是被读。

### 14.4 feeder 自适应

合适的 feeder 数量取决于工作负载和文件系统，没有任何静态公式知道答案：几个大文件只需
一个（一个文件就能填满队列），而一个四万个小文件的目录受限于目录锁，加线程只会更糟。

所以 feeder 数量是一个**许可数**（`FeedGate`：`Mutex<usize>` 加 `Condvar`），拿不到
许可的线程挂起而不是自旋。控制器每 600 ms 采样一次吞吐 —— 大文件按字节，小文件按文件
数 —— 然后爬山：

```
吞吐变好 → 记住这个值
吞吐下降 ≥8% → 退回最优值并反向
```

调整 12 次或进度到 85% 就停手，避免尾部抖动。20 000 个 4 KiB 文件的实测：

| feeder 策略 | 耗时 |
|---|---|
| 固定为 2 | 15.7 s |
| **自适应** | **12.3 s / 13.7 s** |

## 15. 桌面应用

GUI 是同一引擎之上的 iced 前端，并且刻意很薄。界面与引擎之间只有两条通道，都不消耗线程：

1. **计数器。** GUI 分配一个 `Arc<Stats>` 交给 `session::run_with(cfg, src, dst, Some(stats))`。
   引擎既有的 `fetch_add` 落在那块内存上，界面每 90 ms 读一次快照。这和 `--procs`
   跨进程共享的是同一个结构体，只是这里跨线程。
2. **文件事件。** `EngineConfig.events: Option<Sender<Event>>` 传递 `Started`、`Done`、
   `Skipped`、`Failed`、`Finished`。界面在同一个 tick 里排空通道，因此不需要接收线程。

为了画流水线，`Counters` 增加了三个 gauge：`readers_active`、`writers_active`（提交时
加一、完成时减一）和 `queued`。

### 15.1 流水线视图回答"瓶颈在哪"

第一版画的是 reader 圆点、队列条和 writer 圆点，加了流动高光。它没有图例，空闲时看起来
也在忙，而且回答不了任何实际问题。

现在是三根占用率横条加一个判定：

| 条件 | 结论 |
|---|---|
| 写满 + 队列积压 | `destination-bound` |
| 读满 + 队列空 | `source-bound` |
| 读、写、队列都低但字节在动 | `metadata-bound` —— 扫描或建目录是约束 |
| 管线全空但字节在动 | `small files · kernel path` |

最后一行最重要。小文件走 `CopyFile2` / `copy_file_range`，chunk 管线本来就是空的。
三根空条会被读成故障；说清楚"这条路径不经过管线"才是诚实的版本。

### 15.2 事件只走一扇门

早期版本只在"内核拷贝快路径"上发 `Done`，于是走管线的文件永远不离开 IN FLIGHT 列表，
列表涨到 64 行上限后就不再更新。修法是结构性的：事件从 `complete_file()` 发出 —— 每个
文件都必须经过的那个函数，无论是全量、追加、块增量、索引命中还是外部完成。失败原因存
在 `FileCtx.err` 里，`Shared::fail(&file, msg)` 同时写进文件本身和汇总列表，所以行内看
得到原因，报告里也仍然有完整列表。

### 15.3 四个布局坑

1. **滚动条是覆盖在内容上的**，不挤占宽度，所以右对齐的文件大小会丢掉最后 8 px ——
   `2.24 KiB` 显示成 `2.24 K`。修法是给滚动内容加 10 px 右留白。
2. **`Button` 不居中**，它把子组件放在 padding 原点。整宽按钮必须包一层
   `container(...).center_x(Fill)`，否则文字贴在左边缘。
3. **`Space` 在 `Row` 里宽度为 0**，所以横向间隔要显式给宽度。症状是标题渲染成
   `THROUGHPUTlast 60 s`。
4. **`Counters.queued` 是两个队列之和**，而 `queue_depth` 是每个队列的容量，所以仪表
   旁边显示的总容量要乘二 —— 否则一次健康的运行会显示 `128 / 64`。

### 15.4 配色按对比度定，不按感觉

调色板是三级文本色，各自对背景实测：正文约 15:1，次要 8:1，标签 5:1 —— 都高于各自字号
下的 WCAG AA。第一版标签是 2.46:1、描边 1.24:1，在深色背景上那不是一个层次，而是没有
层次。

颜色只用于表意：一个强调色，加上运行 / 完成 / 失败的语义色。没有任何东西为了装饰而上色。

### 15.5 GUI 刻意不做的事

GUI 暴露引擎的选项，但有三类留在命令行，因为窗口不是表达它们的合适工具：

- **多进程切分。** 它带着清单重新执行二进制，属于批处理领域；四条进程之上的进度条并不比
  父进程的汇总更有信息量。
- **没有预览的破坏性模式。** `delete` 没有按钮。GUI 里的删除按钮是一场等着发生的误操作；
  `--dry-run` 就是为这件事存在的。
- **详细程度。** `-v` / `--json` 是给脚本用的输出开关，而 GUI 的输出就是那个窗口。

其余一切 —— I/O 形状、调度、特权、保真、过滤、块索引、增量调参 —— 都能在左侧栏里找到，
并按"常用可见、其余一次点击"的方式分组。

### 15.6 多源是"依次执行"，不是"合并计划"

界面接受一个源列表：文件、文件夹，或者两者混在一起。每一个都是独立的会话 ——
`[a, b, c] → dst/a, dst/b, dst/c` —— 因为 `run_with` 只有一个源根和一个目标，日志、
`Move` 的重命名路径、卷比较全都建立在这个前提上。

另一种做法是把多份扫描合并成一份计划，那意味着管线里同时存在多个源根：日志键要按条目
带上根、重命名要逐条判断是否合法、卷比较要从"每次运行一次"变成"每个条目一次"。而按
序列执行的代价只是每个源多一次扫描和一次管线启动，相对拷贝本身可以忽略。

算术由界面负责：`Totals` 保存已完成会话的计数，每帧与实时的那一份合并。所以进度条和
FILES 卡描述的是整个请求，第二个源开始时不会被打回零。

由此产生三个在界面上看得见的细节：

- 只有源多于一个时，活动流的行才带源名前缀。两个源都可能含 `a/b.txt`；只有一个源时，
  前缀只会把路径里真正有用的部分挤出这一行。
- 停止请求是一个共享标志，在每个源之间检查，作为引擎进程级中断的补充。每个会话开始时
  都会重置那个中断，所以在源边界按下的停止本来会被吞掉。
- `into` 是单个文件夹的 `cp -r` 语义，所以只有在请求恰好是一个文件夹时才向下传递；
  多个源时，嵌套已经由目标路径的拼接天然表达。

### 15.7 测量目标盘

增量成本模型要判断"比较块"是否比"拷贝块"便宜，这个判断需要两侧的读写速率。没有测量时
它按两端都是普通 SATA SSD 估算：这能在速度相同的两个卷之间正确否决增量，但认不出比源慢
十倍的目标 —— 而那正是增量划算的场景。

测量写 64 MiB 再读回来。三个性质决定了这个数字能代表什么：

- **测在真正会写入的位置。** 目标经常是还不存在的文件夹，或者是单文件拷贝的目标文件本身；
  测量在它或其之上最近的已存在目录里进行，那才是字节最终落下的卷。
- **绕开页缓存。** `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` 加对齐缓冲，读回来的是设备。
  由缓存服务的读在任何机器上都能报出几 GB/s，那会让模型以为比较是免费的。拒绝直接 I/O
  的文件系统走缓冲路径，此时读速用写速代替：一个实测的设备速率，而不是乐观的估计。
- **不留痕迹。** 临时文件在每条退出路径上删除，包括出错；失败时显示原因而不是一个布尔值。
  目标一改，结果就作废，因为它描述的是另一个卷。

### 15.8 特权与提权

引擎有三条路径需要特权：目标文件的即时分配（`SetFileValidData` / `fallocate`）、
缓冲区池的大页、以及读取 ACL 拒绝你的文件（备份语义）。

Windows 上，特权必须先在进程令牌里、再在令牌上启用。普通用户会话的令牌里根本没有
`SeManageVolumePrivilege` 和 `SeLockMemoryPrivilege`，所以"启用"这个动作无论重复多少次
都是空操作。因此左侧栏如实显示四个里拿到了几个 ——
`1 of 4 privileges enabled: SeLockMemoryPrivilege` —— 并提供唯一能改变答案的一步：
通过 shell 的 `runas` 动词重启可执行文件，把源和目标作为参数带过去，让表单原样保留。

Linux 上能力来自 exec 时的凭据，没有"启用"这一步，所以左侧栏直接说明要用 `sudo`
启动，而不是提供一个点了也不会改变任何东西的按钮。

无特权路径始终可用：引擎退回基于空洞的扩展和普通页，只是慢，不是错。

### 15.9 只开窗口，不开控制台

Windows 上桌面程序编译成 GUI 子系统的可执行文件，从资源管理器或快捷方式启动时只出窗口，
不出终端。这个属性带 `not(test)` 条件：测试 harness 要往 stdout 打印，而 GUI 子系统的
测试二进制会丢掉这些输出。

没有控制台的进程也没有地方打印 panic 和启动失败，所以 `main` 装了一个 panic hook，把消息
和 backtrace 追加到 `%LOCALAPPDATA%\Sirocco\sirocco-gui.log`；窗口根本建不起来时，
`iced::run` 的错误也写进同一个文件。

## 16. 实测数据

机器：i7-11800H（8C16T），云同步 NVMe（写带宽约 160 MB/s，这是这台机器上的约束）。

基线（Python，buffered + `fsync`）：写 159 MiB/s，缓存读 1293 MiB/s。

| 场景 | 结果 |
|---|---|
| 全量拷贝，IOCP + 直接 I/O + 稀疏预分配 | **159 MiB/s** —— 打满设备写带宽 |
| 全量拷贝，blocking 回退后端 | 138 MiB/s |
| 全量拷贝，`--no-direct`（写进页缓存） | 723 MiB/s —— 缓存数字，不是落盘数字 |
| 用直觉方式做预分配 | 67.8 MiB/s |
| 20 000 个 4 KiB 文件，用户态管线 | 33 s |
| 20 000 个 4 KiB 文件，内核路径 | 17 s（单线程 `robocopy`：17 s） |
| 20 000 个 4 KiB 文件，4 进程 | 7 s（`robocopy /MT:16`：10.5 s） |
| 8 文件 / 78 MiB，`--verify=hash` | 0 失败，逐块校验通过 |
| 200 文件 / 200 MiB | 0 失败 |

结论不是"Sirocco 比 X 快"。结论是：在这台机器上引擎已经贴住设备上限，因此有意义的差别
出现在**协议开销**上 —— 小文件、增量同步、校验 —— 而不是大块吞吐。云同步卷无法体现 NVMe
的绝对性能，但正确性结论（字节级一致的树、校验通过的块、无泄漏）与介质无关。

## 17. 已知限制

这些是设计约束，不是待办清单。

- **不做 CoW 文件系统优化**（btrfs / XFS reflink）。reflink 拷贝瞬时且免费，支持它等于
  让引擎的最佳情况变成"什么都不做"；在那里诚实的答案是 `cp --reflink`。
- **不做"小文件打包成流"**（§6）。对本地拷贝它让 I/O 翻倍。
- **本地拷贝不做用户态压缩**（§7）。成本模型算出它是负收益；唯一的压缩是 NTFS 透明压缩，
  且只在测量支持时才开。
- **ACL 是 opt-in，且测试覆盖薄**（§13）。两个平台上的 `--acl` 路径都已接线，但回归测试
  只覆盖到权限位；POSIX ACL 条目与 Windows DACL 继承没有覆盖。
- **`--delta` 在同等速率的介质之间不会启用**（§5.1）。这是模型按设计工作，不是缺陷。
- **索引信任时间戳**（§8）。外部改写但恢复了 mtime 的文件不会被发现；`--index=verify`
  会重读目标来检查。
- **ARM64 未在硬件上验证。** 代码从 `libc::SYS_*` 取系统调用号，并为没有指令的 CPU 准备
  了查表 CRC32C 路径，但还没有 ARM 机器跑过测试套件。

## 18. 代码地图

```
crates/
  sirocco-sys/          平台层
    src/win/ffi.rs      手写 Win32 绑定
    src/win/iocp.rs     IOCP reactor + OVERLAPPED slab
    src/win/mod.rs      RawFile / 卷信息 / 拓扑 / 特权 / FSCTL
    src/lnx/ffi.rs      io_uring + FIEMAP 常量与结构体
    src/lnx/uring.rs    裸 io_uring 实现
    src/lnx/mod.rs      O_DIRECT / sysfs / affinity / ioprio
    src/blocking.rs     可移植回退（每线程独立句柄）
    src/{align,buf,slab,clock,caps,affinity,interrupt,shm}.rs
  sirocco-core/         算法
    src/delta.rs        增量决策（§5）
    src/entropy.rs      实测可压缩性（§7）
    src/lz.rs           LZ4 block 编解码
    src/hash.rs         XXH64 / CRC32C
    src/queue.rs        有界无锁 MPMC
    src/stats.rs        计数器、快照、速率估计
    src/plan.rs         作业描述类型
    src/pattern.rs      通配符匹配
  sirocco-engine/       编排
    src/session.rs      决策 + 调度 + 汇总
    src/pipeline.rs     readers / writers / feeders
    src/scan.rs         并行目录遍历
    src/journal.rs      断点续传日志
    src/config.rs       全部旋钮与归一化
  sirocco-cli/          参数解析、报告、bench / info / verify
  sirocco-gui/          iced 应用（§15）
tests/
  e2e.py                端到端套件，独立哈希两棵树
  gui_shot.py           启动 GUI 并截取客户区
```
