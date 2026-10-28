# Sirocco 设计文档

> Sirocco（西罗科风）—— 一个从零实现的底层拷贝引擎。
>
> 目标不是"再写一个 cp"，而是在不依赖任何高层异步框架的前提下，
>
> 把 Windows IOCP 与 Linux io_uring、直接 I/O、物理布局感知、增量决策
>
> 全部握在自己手里，并把每一个决策的依据写清楚。

本文档同时是**计划**与**实现说明**：第 1–4 节是设计与取舍，第 5–11 节是

关键算法（含推导），第 12 节是实测数据，第 13 节是已知限制与后续路线。

---

## 1. 命名与定位

| 项    | 值                                                              |
| ---- | -------------------------------------------------------------- |
| 名称   | **Sirocco**                                                    |
| 二进制  | `sirocco`                                                      |
| 版本   | 见 `Cargo.toml` 的 `[workspace.package]`                          |
| 语言   | Rust 2024 edition，`rust-version` 见 `Cargo.toml`                |
| 依赖   | Windows：**零依赖**（手写 `kernel32` / `advapi32` FFI）；Linux：仅 `libc` |
| 明确不用 | Tokio / async-std / windows-sys / clap —— 全部自己写，因为需要的是控制权而不是便利 |

"不依赖"不是洁癖。IO 引擎需要精确控制 `OVERLAPPED` 的生命周期、

`io_uring` 的 SQE 复用、缓冲区注册、亲和性与 I/O 优先级；

任何一层封装都会把这些藏起来，或者在关键路径上多一次分配。

---

## 2. 总体架构

```
┌──────────────────────────────────────────────────────────────────────┐
│ sirocco-cli        参数解析 / 报告 / bench / info / verify            │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-engine     会话编排、扫描、决策、流水线、日志                 │
│   ├─ scan      并行目录遍历（work-stealing）                          │
│   ├─ session   决策（跳过/全量/增量/追加）+ 调度 + 汇总               │
│   ├─ pipeline  reader×R / writer×W，各自持有 reactor                  │
│   ├─ journal   断点续传                                              │
│   └─ config    全部旋钮                                              │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-core       与 OS 无关的算法                                   │
│   ├─ delta     增量决策（成本模型 + Wilson 上界）                     │
│   ├─ entropy   可压缩性实测 + 压缩收益判定                            │
│   ├─ lz        LZ4 block 编解码（自研，wire 兼容）                    │
│   ├─ hash      XXH64 / CRC32C（SSE4.2）                              │
│   ├─ queue     有界无锁 MPMC（Vyukov）                                │
│   ├─ plan      作业描述类型                                          │
│   └─ pattern   include/exclude 通配符匹配                            │
├──────────────────────────────────────────────────────────────────────┤
│ sirocco-sys        平台层：只暴露 RawFile / Reactor / Topology        │
│   ├─ win/       CreateFileW + IOCP + FSCTL + SetFileValidData         │
│   ├─ lnx/       open(O_DIRECT) + 裸 io_uring + FIEMAP + fallocate     │
│   └─ blocking   可移植回退（每线程独立句柄）                          │
└──────────────────────────────────────────────────────────────────────┘
```

**关键设计约束**：`Reactor` 的实现由**单个线程独占**。

Windows 上句柄只能关联一个完成端口，因此"每线程一套句柄"不是优化而是必然；

Linux 上每个线程一个 ring，正好对称。这条约束让上层代码只写一遍。

---

## 3. 平台层

### 3.1 Windows

| 能力       | 机制                                                                                                     |
| -------- | ------------------------------------------------------------------------------------------------------ |
| 直接 I/O   | `FILE_FLAG_NO_BUFFERING` + 4 KiB 对齐的池化缓冲                                                               |
| 异步       | `FILE_FLAG_OVERLAPPED` + `CreateIoCompletionPort` + `GetQueuedCompletionStatusEx`（批量收割）                |
| 完成记录     | `OVERLAPPED` 放在预分配的连续 slab 里，指针算术反查下标，零分配                                                              |
| 错误翻译     | `OVERLAPPED.Internal` 是 NTSTATUS，用 `GetOverlappedResult` 转成 Win32 错误码                                  |
| 预分配      | `SetFileValidData`（需 `SeManageVolumePrivilege`），否则走稀疏空洞（见 §9）                                          |
| 稀疏       | `FSCTL_SET_SPARSE`（可置位/清除）、`FSCTL_SET_ZERO_DATA` 打洞                                                    |
| 物理布局     | `FSCTL_GET_RETRIEVAL_POINTERS`（VCN→LCN）；MFT 序号取 `FileIdInfo` 的低 64 位                                   |
| 介质类型     | `IOCTL_STORAGE_QUERY_PROPERTY`：`StorageDeviceSeekPenaltyProperty` / `StorageAccessAlignmentProperty`   |
| 压缩       | `FSCTL_SET_COMPRESSION`（NTFS 透明压缩）                                                                     |
| 拓扑       | `GetLogicalProcessorInformationEx(RelationAll)`，解析 `PROCESSOR_RELATIONSHIP` / `NUMA_NODE_RELATIONSHIP` |
| 亲和性      | `SetThreadGroupAffinity`（处理器组感知，>64 逻辑核也正确）                                                            |
| I/O 优先级  | `SetFileInformationByHandle(FileIoPriorityHintInfo)` —— **按句柄**，不是按线程                                  |
| 进程/线程优先级 | `SetPriorityClass` / `SetThreadPriority`                                                               |
| 大页       | `VirtualAlloc(MEM_LARGE_PAGES)`，需 `SeLockMemoryPrivilege`                                              |
| 特权       | `OpenProcessToken` + `AdjustTokenPrivileges`（`AdjustTokenPrivileges` 即使失败也返回成功，必须再看 `GetLastError`）    |

### 3.2 Linux

| 能力      | 机制                                                                            |                                      |
| ------- | ----------------------------------------------------------------------------- | ------------------------------------ |
| 直接 I/O  | `open(O_DIRECT)`                                                              |                                      |
| 异步      | 手写 `io_uring`：`io_uring_setup` / `enter` / `register` 三个裸系统调用，自己 mmap SQ/CQ 环 |                                      |
| 优化标志    | \`IORING_SETUP_COOP_TASKRUN                                                   | SINGLE_ISSUER`，可选 `IOPOLL`/`SQPOLL\` |
| 固定缓冲    | `IORING_REGISTER_BUFFERS` + `READ_FIXED`/`WRITE_FIXED`（省掉每次 `get_user_pages`） |                                      |
| 预分配     | `fallocate`，打洞 \`fallocate(PUNCH_HOLE                                         | KEEP_SIZE)\`                         |
| 物理布局    | `FS_IOC_FIEMAP`                                                               |                                      |
| 介质类型    | `/sys/block/<dev>/queue/rotational`                                           |                                      |
| 拓扑      | `/sys/devices/system/cpu/cpuN/topology/*` + `sched_getaffinity`               |                                      |
| 亲和性     | `sched_setaffinity`                                                           |                                      |
| I/O 优先级 | `ioprio_set`（BE 类，映射 0..4 → data 7..0）                                        |                                      |
| 大页      | `mmap(MAP_HUGETLB)`，失败回退 + `madvise(MADV_HUGEPAGE)`                           |                                      |
| 缓存控制    | `posix_fadvise(SEQUENTIAL/DONTNEED)`                                          |                                      |
| 特权      | 只探测 `/proc/self/status` 的 `CapEff`（Linux 的能力在 exec 时授予，运行时无法提权）               |                                      |

### 3.3 可移植回退

`--engine=blocking` 用线程池 + `pread/pwrite`。它存在两个理由：

参考实现（用来交叉验证快速路径），以及 native 后端不可用时的兜底。

> **实测踩到的坑**：这个后端最初让多个工作线程共享一个 `RawFile` 句柄，
>
> 结果在这台机器的云盘上**并发 `ReadFile` 会随机返回 0 字节**
>
> （`OVERLAPPED` 里的偏移被忽略、退化成共享文件指针）。
>
> 换成"每工作线程独立打开句柄"后完全正常。
>
> 现在 IOCP 后端本来就是这样，所以只有 blocking 后端需要这个修正。

---

## 4. 数据通路

```
 scheduler ──Chunk──▶ read queue ──▶ [reader × R] ──Ready──▶ write queue ──▶ [writer × W] ──▶ device
    │                                     │                                        │
    │                                  buffer pool ◀───────────────────────────────┘
    │                                     │
    └─ prepare_dst ─ 创建 / 预分配 / 压缩属性 ─┘
```

- **Chunk** = `(FileCtx, src_off, rlen, dst_off, wlen, len, tail)`。

  一个 1 MiB 的块就是一次 read + 一次 write，不切更碎（除非 `--io-unit` 更小）。
- **reader** 只碰源文件句柄，**writer** 只碰目标文件句柄。

  reader 线程与 writer 线程被钉到**不相交的物理核**上（`--pin`）。
- **buffer pool** 一次性 `VirtualAlloc`/`mmap` 出一大块（可选 2 MiB 大页），

  切成固定大小，原子空闲表分配。缓冲区地址天然满足直接 I/O 对齐。
- **背压**：两个队列都是有界无锁 MPMC（Vyukov），满了就 `yield`，

  不睡眠、不丢数据、不无界增长。

### 4.1 尾部与对齐

直接 I/O 要求偏移、长度、缓冲区地址全部按扇区对齐，而文件长度几乎从不恰好是

4 KiB 的整数倍。处理方式：

1. 目标文件预分配（向上取整到 4 KiB）。
2. 数据块按 4 KiB 向上取整写出，缓冲区尾部补零。
3. 最后一个块**用另一个非直接 I/O 句柄读**（直接 I/O 读越界会失败），

   只读真实长度，再把缓冲区尾部清零。
4. 全部写完后 `SetEndOfFile` / `ftruncate` 截断到精确长度，并恢复 mtime。

句柄缓存按 `(文件 id, 用途)` 索引，用途区分 `src / src_buffered / dst / verify / verify_buffered`。

**这个区分是必需的**：缓存键如果不区分"直接/缓冲"，尾部块会拿到一个

`NO_BUFFERING` 句柄去做非对齐读，直接 `ERROR_INVALID_PARAMETER`。

---

## 5. 增量同步决策算法

这是整个项目里唯一真正"设计"出来的东西，也是问题描述里最含糊的部分

（"根据不充分的信息评估是否应该增量"）。

### 5.1 三种策略与成本

设文件大小 `S`，差异块比例 `r`（未知），读/写/目标读吞吐 `R`、`W`、`Rd`：

| 策略        | 时间                   |
| --------- | -------------------- |
| 全量覆盖      | `S/R + S/W`          |
| 块级增量（无索引） | `S/R + S/Rd + r·S/W` |
| 追加（前缀相同）  | `(S−D)/R + (S−D)/W`  |

块级增量优于全量的条件：

```
S/R + S/Rd + r·S/W  <  S/R + S/W
      ⟺  r < 1 − W/Rd
```

**这个不等式解释了为什么"增量同步"在快盘上经常是个坏主意**：

NVMe 上 `W ≈ Rd`，阈值退化到 0，任何情况都不该增量。

只有在目标比自己的读路径慢得多时（网络盘、USB、SMR、写放大敏感的介质）

才有意义。工具不应该靠猜——所以 Sirocco **测量**两边的速率，

把数字代进不等式，让不等式做决定。

### 5.2 不充分信息下的估计

不知道 `r`，那就抽样。取 `k` 个块对齐的探针（确定性伪随机 + 首尾各一），

两边都读，统计不同的个数 `c`。

只有点估计 `c/k` 是不够的：1000 个块里改 1 个，64 次抽样大概率一次都命中。

所以用 **Wilson 置信区间上界**（95%）：

```
p̂ = c/k,  z = 1.96
上界 = ( p̂ + z²/2k + z·√(p̂(1−p̂)/k + z²/4k²) ) / (1 + z²/k)
```

用上界而不是点估计，是因为两类错误的代价不对称：

误判成"增量"只是多读一遍目标；误判成"全量"则白写一遍全部数据。

上界把判断推向保守的一侧。

### 5.3 子块探针的修正

探针粒度是 64 KiB，比较粒度是 1 MiB。一个 1 MiB 块只要有**任何**一个子块变了就算变，

所以：

```
r_block = 1 − (1 − r_probe)^(block/probe_unit)
```

`inflate = 16` 就是这个指数。没有这一步，估计会系统性偏低，

在"每 1 MiB 改 4 KiB"这种常见场景下会严重高估增量的收益。

### 5.4 追加是真正的赢家

`dst_size < src_size` 且抽样显示前缀未变时，走追加：只读、只写尾部，

**完全不需要碰公共前缀**。对日志、VM 镜像、只追加的数据集，这就是

"第二次同步几乎免费"的来源。

追加不是盲信：提交前会**逐字节校验目标最后 4 MiB** 与源是否一致，

不一致就退回全量。

### 5.5 决策表

| 条件                                               | 动作          |
| ------------------------------------------------ | ----------- |
| 目标不存在                                            | 全量          |
| `size` 相同且 mtime 精确相同（或双方都是整秒且差 ≤2 s，用于 FAT/SMB） | 跳过          |
| `size < min_size`（默认 8 MiB）                      | 全量（探针开销不划算） |
| 目标更小 + 前缀抽样一致 + 尾部 4 MiB 逐字节一致                   | **追加**      |
| `r_upper < (1 − W/Rd) − margin`                  | **块级增量**    |
| 其它                                               | 全量          |


> **时间戳比较的修正**：rsync/robocopy 无条件给 2 秒容差，
>
> 这在"改完立刻同步"的工作流里会静默漏掉文件（本项目 e2e 测试就是这么抓到的）。
>
> 现在的规则是：**先精确比较**；只有当双方时间戳的亚秒部分都为 0
>
> （粗粒度文件系统的特征）时才启用 2 秒容差。

---

## 6. 小文件聚合

小文件的问题不是带宽，是**每文件延迟**：一次 open + read + write + close

在好的盘上也要几十微秒，串行处理几十万个小文件就是几十秒。

Sirocco 的做法是把小文件**在队列层聚合**：

`--small` 阈值以下的文件成批进入流水线，同一批的读/写请求同时在飞，

设备队列深度立刻被填满。配合 96 个句柄/线程的 LRU 缓存，

同一目录下的大量小文件几乎不重复 open。

> 没有采用"打包成一个流再拆包"的方案：对普通文件拷贝，那需要**第二遍**
>
> 把数据拆回各自文件，总 I/O 翻倍。真正的"束"只在归档/容器格式里才有意义。

---

## 7. 压缩决策

在本地盘上，"先压缩再写"通常是**负优化**：压缩省下的是写入量，

但代价是 CPU 时间，而最终还是要写同样多的字节（除非文件系统自己压缩）。

所以 Sirocco 做的是：

1. 抽 3 段共 768 KiB 样本（首/中/尾）。
2. **真的跑一遍 LZ4**，得到实测压缩比 `ratio` 和实测压缩吞吐 `C`。

   （熵只是附带信息：JPEG 和编译产物熵都接近 8，但后者能压 2:1。）
3. 用同一个成本模型判断：
   ```
   ratio·S/W + S/C  <  S/W   ⟺   ratio < 1 − W/C
   ```
   3 GB/s 的 NVMe 配 600 MB/s 的 LZ4 永远不成立 → 自动关闭。

   150 MB/s 的 HDD 上 `ratio < 0.75` 立刻成立 → 打开。
4. 判定为"值得"时，对目标文件开 **NTFS 透明压缩**（`FSCTL_SET_COMPRESSION`），

   由内核压缩，用户态不重复劳动。

自研的 LZ4 block 编解码器（wire 兼容，*约&#x20;*&#x33;00 行）承担两件事：

上面第 2 步的**真实测量**，以及为将来的容器/网络传输格式做准备。

它有自己的 round-trip 测试（随机、全零、文本、重叠匹配、损坏输入）。

---

## 8. 校验

| 模式           | 做法                                     | 成本      |
| ------------ | -------------------------------------- | ------- |
| `none`       | 不校验                                    | 0       |
| `size`       | 比较最终大小                                 | 一次 stat |
| `size-mtime` | 大小 + mtime                             | 一次 stat |
| `hash`       | 每个块：reader 算源块哈希 → writer 写完回读目标块 → 比对 | 读回一遍目标  |
| `sample`     | 首/中/尾块                                 | 极低      |

哈希默认 XXH64（~13 GB/s，远高于任何存储设备），可选 CRC32C（SSE4.2 指令）。

校验在 writer 线程内完成，天然并行，且能定位到**具体哪个块**出错。

---

## 9. 空间预分配：一个必须写下来的坑

直觉上"先把文件扩到最终大小"能减少碎片。但在 NTFS 上，

`SetEndOfFile` 扩展出来的区间 **valid data length = 0**，

任何"跳着写"的操作都会触发**从头补齐的零填充**——相当于把数据写了两遍。

实测（本机 128 MiB 拷贝）：

| 配置                        | 吞吐            |
| ------------------------- | ------------- |
| 预分配 + `SetEndOfFile`（无特权） | 67.8 MiB/s    |
| 不预分配                      | 158 MiB/s     |
| **预分配走稀疏空洞**              | **159 MiB/s** |

所以策略是：

1. 先试 `SetFileValidData`（有 `SeManageVolumePrivilege` 时零成本）；
2. 不行就把文件标成稀疏、再扩展——扩展变成空洞，不产生零填充，

   写完后再**清掉稀疏标志**（`FSCTL_SET_SPARSE` 传 `SetSparse=FALSE`），

   这样输出文件在属性上和普通文件没有区别。

特权探测只在会话开始时做一次（在目标目录建一个临时文件试一次），

而不是每个文件都试一遍——否则每个文件都要先扩展、再发现失败、再回退。

`--prealloc=auto|valid|sparse|off` 可以强制指定。

---

## 10. 调度、亲和性与 NUMA

- `readers = 物理核数 / 2`，`writers = 其余`（`--threads` 可覆盖）。
- `--pin` 时，reader 和 writer 分别钉到**不相交的物理核**；

  Windows 用处理器组掩码，Linux 用 `sched_setaffinity`。

  这样 R/W 两个线程不会落在同一个 SMT 兄弟核上抢执行单元。
- `--io-priority` 在 Windows 上是**按句柄**设置的（`FileIoPriorityHintInfo`），

  在 Linux 上是 `ioprio_set` 的 BE 类。
- `--process-priority` / `--thread-priority` 分别控制进程类与线程优先级；

  Linux 上映射到 nice 值。
- NUMA：拓扑里记录了每个逻辑核所属的 node，当前版本用于**报告与线程放置**；

  显式的 `mbind`/`SetThreadPreferredNode` 留在路线图里（见 §14）。

---

## 11. HDD 物理顺序

机械盘上顺序访问和随机访问差两个数量级。Sirocco 提供两层：

1. **文件级**：`--order=physical` 按物理布局键排序——

   Windows 取 NTFS 的 MFT 记录号（`FileIdInfo` 低 64 位），

   Linux 取 inode 号。这近似于数据在盘片上的物理顺序。

   （代价是每个文件多一次 open，所以是 opt-in。）
2. **块级**：`physical_extents()` 通过 `FSCTL_GET_RETRIEVAL_POINTERS` / `FIEMAP`

   拿到 VCN→LCN 映射，用于识别碎片化文件。

介质类型由 `IncursSeekPenalty` / `queue/rotational` 判定，

`--order=physical` 只对旋转介质有意义，SSD 上它是纯损失。

---

## 12. 实测数据（本机：i7-11800H 8C16T，云盘 NVMe）

基线（Python，buffered + fsync）：写 159 MiB/s，缓存读 1293 MiB/s。

| 场景                                | 结果                     |
| --------------------------------- | ---------------------- |
| 全量拷贝（IOCP + 直接 I/O + 稀疏预分配）       | **159 MiB/s**（打满设备写带宽） |
| 全量拷贝（blocking 回退后端）               | 138 MiB/s              |
| 全量拷贝（`--no-direct`，写进页缓存）         | 723 MiB/s（非落盘数字）       |
| 预分配不当时                            | 67.8 MiB/s             |
| 8 文件 / 78 MiB 树 + `--verify=hash` | 0 失败，全量校验通过            |
| 200 文件 / 200 MiB                  | 0 失败                   |

结论：**在这台机器上，瓶颈是设备本身**，引擎已经贴住设备上限。

真正的差距体现在小文件、增量同步和校验这些"协议开销"场景。

> 说明：云盘的写带宽只有 ~160 MB/s，因此本机无法体现 NVMe 上的绝对性能。
>
> 引擎的正确性结论（字节级一致、校验通过、无泄漏）与介质无关。



---

## 13. 权限分级与深度系统特性

### 13.1 能力探测：不要问用户，问系统

每一个有价值的系统特性都挂在权限上。与其让用户记一堆 flag，不如启动时**实测**一遍：

Windows 侧逐个 `AdjustTokenPrivileges` 看哪个真的生效，Linux 侧读 `CapEff`。

结果是一个 `Caps` 结构体，整个引擎按它分叉。

| 能力              | Windows 依据                        | Linux 依据              | 解锁的路径                    |
| --------------- | --------------------------------- | --------------------- | ------------------------ |
| `instant_alloc` | `SeManageVolumePrivilege`         | `fallocate`（无需特权）     | `SetFileValidData`，跳过零填充 |
| `large_pages`   | `SeLockMemoryPrivilege`           | `CAP_IPC_LOCK`        | 2 MiB 页缓冲池               |
| `backup`        | `SeBackupPrivilege`               | `CAP_DAC_READ_SEARCH` | 打开 DACL 拒绝的文件            |
| `restore`       | `SeRestorePrivilege`              | `CAP_FOWNER`          | 恢复属主/权限                  |
| `rt_io`         | —                                 | `CAP_SYS_ADMIN`       | `IOPRIO_CLASS_RT`        |
| `rt_priority`   | `SeIncreaseBasePriorityPrivilege` | `CAP_SYS_NICE`        | 实时线程优先级                  |
| `mlock`         | 工作集配额                             | `CAP_IPC_LOCK`        | 锁定缓冲池页                   |
| `noatime`       | —                                 | 文件属主                  | `O_NOATIME`              |

运行时会打印一行，例如：

```
sirocco: capabilities: unprivileged [large-pages, mlock]
sirocco: allocation strategy: hole-based extension
```

这就是"不同权限下走不同路径"的具体形态：同一个二进制，在普通用户下走空洞扩展，

拿到 `SeManageVolumePrivilege` 后自动改用 `SetFileValidData`。

### 13.2 内核侧拷贝：小文件的正确工具

小文件的瓶颈不是带宽，是每文件的元数据往返。用户态路径要 open + create + write +

set-time + close，内核路径只要一次调用。

| 实现                   | 20,000 个 200 B–4 KB 文件（Windows，云盘） |
| -------------------- | ---------------------------------- |
| 用户态 read/write 管线    | 33 s                               |
| 上述 + 小文件免预分配/免截断     | 31 s                               |
| **`CopyFile2` 内核拷贝** | **17 s**                           |
| `robocopy`（参照）       | 17 s（单线程）/ 10 s（`/MT:16`）          |

所以 `--small-copy=auto`（默认）在**不需要看到数据**时直接交给内核：

没有 `--verify=hash`、没有 `--index`、没有 `--compress`、没有 `--sparse` 才走这条路。

任何需要读数据的特性都会自动退回用户态管线。

Windows 用 `CopyFile2` + `COPYFILE2_EXTENDED_PARAMETERS`（对 UNC 路径加

`COPY_FILE_REQUEST_COMPRESSED_TRAFFIC`，让 SMB 侧压缩传输）；

Linux 用 `copy_file_range`，并补回它不携带的 mtime 与权限位。

### 13.3 硬链接保真

扫描阶段拿到 inode/file id 与链接数（Linux 免费来自 `stat`；Windows 用

`NtQueryInformationFile(FileStandardInformation)` + `FileIdInfo`）。

第一个路径正常拷贝，其余同 inode 的路径直接 `CreateHardLinkW` / `link(2)`，

不再复制数据。`--links` 默认开启，`--no-links` 关闭。

并发下兄弟文件可能同时在飞，所以链接失败会短暂重试，仍失败则退回复制——正确性不受影响。

### 13.4 元数据保真

| 平台      | 机制                                                           |
| ------- | ------------------------------------------------------------ |
| Windows | `SetFileInformationByHandle(FileBasicInfo)` 一次写全属性；mtime 同调用 |
| Linux   | `fchmod` 通过句柄写权限位；`--xattrs` 时逐个 `fgetxattr`/`fsetxattr`     |

这一步补上了此前的真实缺口：**可执行位会丢**。`permissions_are_preserved` 测试锁住它。

### 13.5 稀疏源跳读

`--sparse` 现在分两层：

1. **源侧**：用 `FSCTL_QUERY_ALLOCATED_RANGES` / `SEEK_DATA`+`SEEK_HOLE` 拿到源文件的
     
   已分配区间。整块落在未分配区的 chunk **根本不读**——直接给 writer 发一条
     
   `Hole` 消息，让它在目标上打洞。这类 chunk 连缓冲区都不占用。
2. **目标侧**：剩下的 chunk 若内容全零（且源没报告分配信息），仍按老办法打洞。

实测（Linux，64 MiB 文件只分配了 3 MiB）：

```
data : 3.00 MiB written, 3.00 MiB read, 0 B verified, 63.9 MiB punched
```

注意打洞后**不能**再清除目标文件的稀疏标志——那会把洞实体化成零簇，白干一场。

### 13.6 按 LCN 重排同文件块

机械盘上顺序访问和随机访问差两个数量级。文件级排序（`--order=physical`）解决
  
"文件之间的顺序"，文件内部还需要一层：把同一文件的 chunk 按**物理簇号**排序，
  
让读请求按数据在盘片上的落点发出。

`FIEMAP` / `FSCTL_GET_RETRIEVAL_POINTERS` 给出 `(vcn, lcn, len)` 区段，
  
chunk 的起始偏移映射到 LCN 就是一次查表。空洞排在最后（没有东西可寻道）。

只在 `queue/rotational == 1` 时默认启用（`--lcn-order` / `--no-lcn-order` 可强制）。
  
SSD 上它是纯开销。

### 13.7 ACL

| 平台      | 机制                                                                                                                   |
| ------- | -------------------------------------------------------------------------------------------------------------------- |
| Windows | `GetSecurityInfo(SE_FILE_OBJECT, OWNER\|GROUP\|DACL)` → `SetSecurityInfo`。设属主需要 `SeRestorePrivilege`，没有时自动降级为只写 DACL |
| Linux   | POSIX ACL 就是扩展属性（`system.posix_acl_access` / `default`），复用 xattr 通道，不引入 libacl                                       |

`--acl` 开启（默认关闭，因为跨文件系统复制 ACL 经常失败，显式选择更诚实）。
  
内核拷贝路径（`CopyFile2`）本来就会带 DACL；这条补的是用户态管线路径。

### 13.8 粗粒度多进程

`--procs=N` 不在每个文件上拆进程——那样只会让多个进程抢同一个目录锁。
  
拆分发生在**子树粒度**：父进程扫描后按字节数把连续的遍历区间切成 N 份，
  
先一次性建好所有目录，再给每个 worker 一个清单（`--files-from`），各自独立运行。

```
$ sirocco copy src dst --procs=4
sirocco: split into 4 processes (20000 files)
sirocco split summary (4 processes)
  files        : 20000 copied, 0 failed
  elapsed      : 7s
```

同一棵 20,000 个小文件的树：单进程 14.5 s → 4 进程 **7 s**
  
（robocopy `/MT:16` 是 10.5 s）。收益来自绕开目录锁，不是来自更多线程。

### 13.9 跨进程进度：把计数器放进共享内存

直觉方案都有代价：管道每条更新一次 syscall；socket 要一个线程；共享内存 + 发布线程
  
仍然要一个线程。而这条路径每秒钟要跑几十万次。

真正的零成本做法是**让计数器本身就住在共享内存里**：`Counters` 是
  
`#[repr(C)]` 的全 `AtomicU64` 结构体，子进程把 `Stats` 直接建在映射区上，
  
于是流水线原本就在做的 `fetch_add` 全部落进父进程能读的页里。

- 父进程每个 worker 建一个映射文件（`<tmp>/stats-<i>.shm`），
    
  通过 `--shm=<path>` 传给子进程；
- 子进程 `Stats::from_shared(ptr)`，之后所有计数天然可见；
- 父进程每 250 ms 读一遍这些缓存行求和，渲染进度条。

**没有额外线程，没有 IPC，没有 syscall。** 唯一的新增成本是父进程的 4 次/s 读取。

一个 `magic` 字段放在结构体首位，避免读到陈旧或不相干的映射。

### 13.10 feeder 自适应

feeder 的合适数量取决于工作负载和文件系统，没有任何静态公式知道答案：

- 几个大文件 → 1 个就够（一个文件就能把队列填满）；
- 一个目录几万个 4 KiB 文件 → 瓶颈是目录锁，加线程只会更慢。

所以 feeder 数量变成一个**许可数**（`FeedGate`：`Mutex<usize>` + `Condvar`），
  
线程在拿不到许可时挂起（不占 CPU）。控制器每 600 ms 采样一次吞吐
  
（大文件按字节、小文件按文件数），做一步爬山：

```
rate 变好 → 记住这个值
rate 变差 8% 以上 → 退回最优值并反向
```

最多调整 12 次或进度过 85% 就停手，避免尾部抖动。

实测（20,000 个 4 KiB 文件，Windows 云盘）：

| feeder 策略 | 耗时                  |
| --------- | ------------------- |
| 固定 2      | 15.7 s              |
| **自适应**   | **12.3 s / 13.7 s** |

### 13.11 其余被"用起来"的底层接口

| 接口                                                | 用途                                            |
| ------------------------------------------------- | --------------------------------------------- |
| `NtSetInformationFile(FileEndOfFileInformation)`  | 截断：一次调用替代 `SetFilePointerEx` + `SetEndOfFile` |
| `NtQueryInformationFile(FileStandardInformation)` | 一次拿到大小、分配大小、链接数                               |
| `FILE_ALLOCATED_RANGE_BUFFER`                     | 查询已分配区间（稀疏源检测）                                |
| `statx(STATX_DIOALIGN)`                           | Linux 上 `O_DIRECT` 的真实对齐要求，替代硬编码 4096         |
| `VirtualLock` / `mlock`                           | 锁定缓冲池页，避免提交路径上的软缺页                            |
| `CancelIoEx`                                      | 中断时立刻放弃在飞的 I/O，而不是等它做完                        |
| `GetCurrentProcessorNumber` / `sched_getcpu`      | `-vvv` 时验证亲和性真的生效了                            |


同时把 `ffi.rs` 里 20 个从未使用的绑定删掉了（`QueryPerformanceCounter`、

`CreateFileMappingW`、`Sleep` 之类），现在每个绑定都有调用点。

## 13.12 图形界面（iced）

界面和引擎之间只有两条通道，都不需要额外线程：

1. **计数器**：GUI 自己造一个 `Arc<Stats>`，交给 `session::run_with(cfg, src, dst, Some(stats))`。
   引擎原本就在做的 `fetch_add` 直接落在这块内存上，界面每 90 ms `snapshot()` 一次。
   和 `--procs` 的共享内存方案是同一套结构体，只是这里跨线程而非跨进程。
2. **文件事件**：`EngineConfig.events: Option<Sender<Event>>`，
   `Started / Done / Skipped / Failed / Finished` 五种，每个文件两条。
   GUI 在同一个 tick 里排空通道，因此不需要自己的接收线程。

为了画流水线占用，`Counters` 多了三个 gauge：`readers_active`、`writers_active`
（提交时 +1、完成时 -1）和 `queued`（队列深度，跟随 feeder 控制器每 600 ms 采样）。

### 13.12.1 流水线视图回答的是"瓶颈在哪"

一开始画的是"读线程圆点 → 队列条 → 写线程圆点"，加了流动高光。问题有三个：
没有图例、空闲时也像在忙、而且回答不了任何实际问题。

现在改成三根占用率横条加一个判定：

| 条件 | 结论 |
|---|---|
| 写满 + 队列积压 | `destination-bound` |
| 读满 + 队列空 | `source-bound` |
| 读、写、队列都低但字节在动 | `metadata-bound`（扫描/建目录是瓶颈） |
| 管线全空但字节在动 | `small files · kernel path`（小文件根本没走管线） |

最后一条尤其重要：小文件走 `CopyFile2` / `copy_file_range`，chunk 管线是空的。
显示三根空条会让人以为出问题了，写清楚"这条路径不经过管线"才是诚实的。

### 13.12.2 事件必须从唯一的出口发

第一版只在"内核拷贝快路径"发 `Done`，走管线的文件永远不离开 IN FLIGHT，
列表涨到 64 行上限后就不再更新。修法是把事件发在 `complete_file()` ——
所有文件（全量、追加、块增量、索引增量、外部完成）都必须经过的那个函数。
失败原因存在 `FileCtx.err` 里，`Shared::fail(&file, msg)` 同时写进汇总列表和文件本身，
所以行内能看到原因，报告里也有完整列表。

### 13.12.3 界面这一层的四个坑

1. **滚动条是覆盖在内容上的**，不挤占宽度。右侧对齐的文件大小、线程数会被盖掉最后
   8 px —— 表现是 `2.24 KiB` 显示成 `2.24 K`。修法是给滚动内容加 10 px 右留白。
2. **`Button` 不居中**：iced 把子组件放在 padding 原点。整宽按钮必须自己包一层
   `container(...).center_x(Fill)`，否则文字贴在左边缘。
3. **`Space::with_height` 在 Row 里宽度为 0**，所以横向间隔要用 `with_width`。
   这个错误的表现是标题粘成 `THROUGHPUTlast 60 s`。
4. **`Counters.queued` 是两个队列之和**，而 `queue_depth` 是每个队列的容量，
   分母要乘 2，否则健康运行时也会显示 `128 / 64`。

### 13.12.4 配色是按对比度定的，不是按感觉

| 用途 | 颜色 | 在 `PANEL` 上的对比度 |
|---|---|---|
| 正文 | `(0.906, 0.918, 0.941)` | ~15:1 |
| 次要 | `(0.686, 0.714, 0.769)` | ~8:1 |
| 标签/眉题 | `(0.522, 0.553, 0.616)` | ~5:1 |
| 描边 | `(0.270, 0.290, 0.330)` | ~2:1 |

第一版标签色只有 2.46:1、描边 1.24:1 —— 整套层次结构在屏幕上等于不存在，
五张卡片会糊成一片。深色背景上的投影同样是隐形的，所以高度只靠表面色阶表达。

字号只有五档（10 / 12 / 15 / 20 / 24），间距只有四档（4 / 8 / 12 / 14），
圆角只有三档（6 / 8 / 12）。档位少才能让相邻两档的差别看得出来。

### 13.12.5 刻意不做进 GUI 的

| 能力 | 理由 |
|---|---|
| `--procs` | 要重新执行二进制并给每个 worker 传清单，是批处理语义；界面是单任务模型 |
| `delete` 模式 | 一个不需要确认的删除按钮不值得做 |
| 多源 | 单源模型已经能表达绝大多数用法，多源需要列表 UI 和逐项状态 |
| NUMA / 亲和 / 缓冲区 | 需要看机器拓扑才调得动，默认值已经按能力探测自适应 |
| 压缩 / 扩展属性 | 收益场景窄，且都需要额外的数据可见性 |

这些在 CLI 里都还在，README 的参数表是完整清单。

## 14. 命令行

```
sirocco <command> [options] <source>... <destination>

命令：copy / move / sync / delete / verify / bench / info
```

完整的参数表见 `README.md` 与 `sirocco --help`。设计原则：

**每一个影响性能的旋钮都必须可调**，且默认值必须在普通硬件上就是合理的。

---

## 15. 已知限制与后续路线

### 已实现且验证

- 直接 I/O、IOCP 批量收割、无锁队列、池化缓冲区、大页。
- 全量 / 跳过 / 追加 / 块级增量四种决策路径。
- 四种校验模式，块级并行，失败可定位。
- 稀疏打洞、稀疏源跳读、预分配（三种策略）、mtime 保留。
- 断点续传日志、持久化块索引。
- copy / move / sync / delete / verify / bench / info 七个子命令，七种覆盖策略。
- 多进程切分（`--procs`，按子树划分，进度走共享内存）。
- iced 图形界面（`sirocco-gui`，非默认成员）。
- Windows 与 Linux 双平台，均在真机上构建并通过全部测试。
- 73 个 Rust 测试（Linux 上 74，多一个权限位用例）+ 21 个端到端测试。

### 已在真机验证

- **io_uring**：在 Debian sid 真机上跑通全部 Rust 测试与端到端测试。
- **SIGINT**：`timeout -s INT` 中断 2 GiB 拷贝，进程在毫秒级停下并保留已完成的部分。
- **NUMA**：设备节点探测 + 缓冲池 `mbind` + 线程 `set_mempolicy`（单节点机器上无副作用）。
- **目录级并行**：多个 feeder 从共享游标取任务；1/2/4/8 个 feeder 产出的树完全一致。
- **多进程切分**：4 个子进程按子树划分，产出的树与单进程一致。

### 未实现 / 待验证

| 项 | 状态 |
| --- | --- |
| ACL（非属主/权限位） | `--acl` 已接线，但只在权限位层面验证过；POSIX ACL 条目与 Windows DACL 的继承规则没有回归测试。 |
| 多 feeder 的收益 | 在 2 核 / 8 核机器上实测无显著收益（瓶颈在目录锁与设备延迟），默认值保守取 `物理核/4`。 |
| ARM64 真机 | 系统调用号取自 `libc::SYS_*`、CRC32C 有查表回退，但没有 ARM 硬件跑过测试。 |
| 跨卷 `--delta` 的索引复用 | 索引按目标 `(size, mtime)` 记录，跨卷移动后需要重建。 |

### 设计上明确**不做**的事

- **不做 CoW 文件系统优化**（btrfs/XFS reflink）：需求里明确排除。
- **不做"打包成流再拆包"的小文件方案**：对普通拷贝是双倍 I/O。
- **不在普通本地拷贝里做用户态压缩**：见 §7，成本模型会算出来它是负收益。

---

## 16. 代码地图

```
crates/
  sirocco-sys/          平台层
    src/win/ffi.rs      手写 Win32 绑定（~500 行）
    src/win/iocp.rs     IOCP reactor + OVERLAPPED slab
    src/win/mod.rs      RawFile / 卷信息 / 拓扑 / 特权 / FSCTL
    src/lnx/ffi.rs      io_uring + FIEMAP 常量与结构体
    src/lnx/uring.rs    裸 io_uring 实现
    src/lnx/mod.rs      O_DIRECT / sysfs / affinity / ioprio
    src/blocking.rs     可移植回退（每线程独立句柄）
    src/{align,buf,slab,clock,caps,affinity,interrupt}.rs
  sirocco-core/         算法
    src/delta.rs        增量决策（本文 §5）
    src/entropy.rs      可压缩性实测（本文 §7）
    src/lz.rs           LZ4 block 编解码
    src/hash.rs         XXH64 / CRC32C
    src/queue.rs        有界 MPMC
    src/{plan,pattern,human,stats,error}.rs
  sirocco-engine/       流水线
    src/pipeline.rs     reader/writer 线程、句柄缓存、完成处理
    src/session.rs      决策、调度、汇总
    src/scan.rs         并行目录遍历
    src/journal.rs      断点续传
    src/index.rs        持久化块索引
  sirocco-cli/
    src/main.rs         子命令
    src/args.rs         手写参数解析
tests/e2e.py            端到端验收（用 hashlib 独立比对）
```
