<p align="center">
  <img src="logo.svg" alt="Sirocco" width="120">
</p>

<h1 align="center">Sirocco</h1>

<p align="center">
  面向 Windows 与 Linux 的文件拷贝引擎：直接 I/O、内核辅助的小文件拷贝，
  以及在全量拷贝与块级增量之间的实测选择。
</p>

<p align="center">
  <a href="#快速开始">快速开始</a> ·
  <a href="#它到底不一样在哪">它到底不一样在哪</a> ·
  <a href="#图形界面">图形界面</a> ·
  <a href="#参数速查">参数速查</a> ·
  <a href="#测试">测试</a> ·
  <a href="README.md">English</a>
</p>

---

**一个从零实现的底层拷贝引擎**：Windows IOCP、Linux io_uring、直接 I/O、
物理布局感知、增量同步决策、可选校验与透明压缩。不依赖任何异步运行时；
Windows 侧零第三方依赖，Linux 侧只有 `libc`。

它不是 `std::fs::copy` 的封装。每一层都直接写在平台接口上 —— Windows 侧
`CreateFileW` / `ReadFile` / `WriteFile` 走 IOCP，Linux 侧 `open(O_DIRECT)`
加手写 `io_uring`。设计与每个默认值的推导见 **[DESIGN.md](DESIGN.md)**。

```console
$ sirocco copy D:\data E:\backup --verify=hash --pin
sirocco: 12431 files, 812 dirs, 48.2 GiB to copy; buffers 136 × 1.00 MiB = 136 MiB; 4 readers + 4 writers
  ...
sirocco copy summary
  backend      : iocp (align 4096)
  files        : 12431 copied, 0 skipped, 0 failed (of 12431)
  data         : 48.2 GiB written, 48.2 GiB read, 48.2 GiB verified, 0 B saved
  elapsed      : 5m12s
  throughput   : 158 MiB/s (write), 158 MiB/s (read)
```

## 快速开始

```bash
cargo build --release                 # 命令行：target/release/sirocco
cargo build --release -p sirocco-gui  # 图形界面：target/release/sirocco-gui
```

需要满足 `Cargo.toml` 中 `rust-version` 的 Rust 工具链。
除此之外没有别的依赖：没有构建脚本、没有 pkg-config、没有内嵌 C 代码。

> GUI 刻意**不是**默认成员：iced 会拉进 wgpu / winit / wayland / X11，无头构建机上
> 通常装不起来。这样 `cargo build` 和 `cargo test` 在 Windows 上保持零依赖，
> 在 Linux 上只需要 `libc`。

## 它到底不一样在哪

五条，每条都附上支撑它的实测数字。

**1. 默认直接 I/O，不划算时有退路。** `FILE_FLAG_NO_BUFFERING` / `O_DIRECT`
让拷贝绕开页缓存，字节只写一次设备而不是写两次（缓存一次、落盘一次）。同一个任务上，
IOCP 后端实测 159 MiB/s，可移植阻塞后端 138 MiB/s。目标快、源慢时用 `--no-direct`
把缓存放回来 —— 但那个数字（723 MiB/s）是页缓存的数字，不是落盘的数字。

**2. 小文件交给内核。** 一堆 4 KiB 文件上，用户态管线的开销被 open 而不是字节数
主导。小于 `--buffer` 的文件在 Windows 上交给 `CopyFile2`、Linux 上交给
`copy_file_range`。2 万个 4 KiB 文件实测：用户态管线 **33 秒**，内核路径
**17 秒**，与单线程 `robocopy` 持平。任何需要看到数据的特性（`--verify=hash`、
`--index`、`--compress`、`--sparse`）会自动退回管线。

**3. 预分配不会让写入量翻倍。** NTFS 上 `SetEndOfFile` 会零填充扩展出来的部分，
所以"先占空间"要多写一遍。Sirocco 用稀疏空洞扩展、事后清掉稀疏标志：
同一个任务 **67.8 MiB/s → 159 MiB/s**。持有 `SeManageVolumePrivilege` 时改走
`SetFileValidData`，连空洞都不用。

**4. 只在数字支持时才做增量。** 块级增量不是免费的：它要先读目标才知道哪里变了。
Sirocco 探几个块、用 Wilson 置信上界外推，再用**实测**吞吐（不是假设的）比较两条路
的成本。两个同样快的卷之间，它会正确判定全量更便宜，连探针都不做。目标明显更慢时
（网络盘、U 盘），`--calibrate` 会让模型给出你本来就怀疑的结论。

**5. 它会告诉你时间花在哪。** GUI 用三根横条显示 read / queue / write 的占用率，
并直接给出结论 —— `destination-bound` / `source-bound` / `metadata-bound` /
`flowing`。这是吞吐数字回答不了、而拷贝变慢时最先想知道的那个问题。

## 图形界面

```bash
sirocco-gui
```

基于 [iced](https://iced.rs)，单屏：左侧配置，右侧实时状态。

**右侧从上到下**：状态栏 → 四张统计卡（速度 / 文件数 / 已用时 / 剩余）→ 进度条 →
60 秒吞吐曲线与流水线仪表 → 文件活动流。活动流分 **IN FLIGHT**（最多 64 行）与
**RECENT**（最新在上，最多 300 行），失败的文件下面多一行红字原因。

**左侧配置**：模式（Copy / Move / Mirror，后两者显示不可逆警告）、源与目标路径
（可拖放文件夹）、硬链接 / 稀疏打洞 / 权限与 ACL / 块索引 / 断点续传 / 写完落盘 /
仅预览、校验模式、7 种覆盖策略、include / exclude 通配符与扫描顺序、I/O 线程数
（0 = 自适应，标签会显示实际解析成几个读 + 几个写）。

界面与引擎之间只有两条通道，都没有额外线程：GUI 造一个 `Arc<Stats>` 交给
`session::run_with()`，引擎的计数器直接写进那块内存，界面每 90 ms 读一次；
每个文件的事件走一条 `mpsc`，同一次 tick 里排空。

**刻意没有放进界面的**（CLI 仍然可用）：`--procs` 多进程切分、`delete` 模式、
多源、NUMA / 亲和 / 缓冲区调优、压缩、扩展属性。理由在 DESIGN.md §13.12。
`Esc` 停止当前任务。

## 常用组合

```bash
sirocco copy src dst --verify=hash --pin      # 首次全量 + 全量校验，绑核
sirocco sync src dst                          # 日常同步：只改动过的，多余删除
sirocco copy src dst --order=physical --buffer=8M --io-unit=8M --overlap=16
sirocco copy src dst --buffer=256K --io-unit=256K --overlap=32 --threads=16
sirocco copy src dst --journal                # 断点续传：中断后再跑同一条命令
sirocco copy src dst --dry-run -v             # 先看看会发生什么
```

## 参数速查

完整列表见 `sirocco --help`（与解析器同源，不会不同步）。

### I/O 形状

| 参数 | 说明 |
|---|---|
| `--buffer=SIZE` | 池化缓冲区大小（默认 1M） |
| `--io-unit=SIZE` | 单次请求最大字节数（默认 1M，须整除 `--buffer`） |
| `--overlap=N` | 每线程同时在飞的请求数（默认 8） |
| `--readers=N` / `--writers=N` | 读 / 写线程数（默认各取物理核的一半） |
| `--threads=N`, `-j N` | 用 N 个线程，自动分配 |
| `--queue=N` | 流水线队列深度 |
| `--feeders=N` | 调度线程数（默认 0 = 自适应，按实测吞吐爬山） |
| `--cpu-list=LIST` | 只在这些逻辑核上运行（`0-3,8-11`） |
| `--numa=auto\|off\|N` | 缓冲区与线程的 NUMA 策略 |
| `--small-copy=auto\|kernel\|io` | 小文件交给内核还是走管线 |
| `--engine=native\|blocking` | IOCP / io_uring 或可移植线程池 |
| `--no-direct` | 关闭直接 I/O（写进页缓存） |
| `--huge-pages` | 缓冲区用 2 MiB 大页 |
| `--prealloc=auto\|valid\|sparse\|off` | 目标空间预留策略 |
| `--sparse` | 全零 / 未分配区域打洞 |
| `--lcn-order` / `--no-lcn-order` | 同文件块按物理簇排序（HDD） |
| `--procs=N` | 粗粒度多进程：按子树切分，进度走共享内存 |
| `--shm=PATH` | 把进度计数器发布到共享内存段 |
| `--files-from=FILE` | 只拷贝清单里的路径 |
| `--journal` | 断点续传日志 |
| `--sync` | 每个文件写完 fsync |

### 策略

| 参数 | 说明 |
|---|---|
| `--overwrite=POLICY` | `if-different`(默认) / `never` / `always` / `if-newer` / `if-older` / `if-size` / `if-content` |
| `-f, --force` | 等价于 `--overwrite=always` |
| `-n, --dry-run` | 只报告不执行 |
| `--verify=MODE` | `none`(默认) / `size` / `size-mtime` / `hash` / `sample` |
| `--hash=xxh64\|crc32c` | 校验哈希算法 |
| `--compress=off\|auto\|force` | NTFS 透明压缩 |
| `--delta` / `--no-delta` | 增量同步开关 |
| `--delta-block=SIZE` | 增量比较粒度（默认 1M） |
| `--delta-min=SIZE` | 值得探测的最小文件（默认 8M） |
| `--order=walk\|size-asc\|size-desc\|physical\|name` | 处理顺序 |
| `--exclude=GLOB` / `--include=GLOB` | 通配符过滤（可重复） |
| `--exclude-from=FILE` / `--include-from=FILE` | 从文件读模式（`#` 注释） |
| `--into` | `cp -r` 语义：把源目录本身放进目标 |
| `--index[=trust\|verify]` | 持久化块索引，重同步时不再读目标 |
| `--links` / `--no-links` | 是否保持硬链接（默认保持） |
| `--xattrs`, `-A` | 复制扩展属性（Linux） |
| `--acl` / `--no-acl` | 复制 DACL / POSIX ACL |
| `--follow-symlinks` / `--one-filesystem` | 符号链接 / 不跨文件系统 |
| `--delete-extra` | 删除目标端多余文件（`sync` 默认开启） |

### 调度与其它

| 参数 | 说明 |
|---|---|
| `--pin` | reader / writer 钉到不相交物理核 |
| `--io-priority=LEVEL` | `verylow` / `low` / `normal` / `high` / `critical` |
| `--process-priority=LEVEL` | `idle` / `below` / `normal` / `above` / `high` / `realtime` |
| `--thread-priority=LEVEL` | `idle` / `lowest` / `below` / `normal` / `above` / `highest` |
| `--privileged` | 强制走特权路径（默认自动探测） |
| `-v` / `-q` | 更详细 / 安静 |
| `--json` | 机器可读汇总 |
| `--calibrate` | 先测读 / 写吞吐，喂给增量成本模型 |

## 平台支持

| | Windows | Linux |
|---|---|---|
| 异步 I/O | IOCP | `io_uring`（含固定缓冲 / 固定文件，可选 SQPOLL / IOPOLL） |
| 直接 I/O | `FILE_FLAG_NO_BUFFERING` | `O_DIRECT` |
| 物理布局 | `FSCTL_GET_RETRIEVAL_POINTERS` | `FIEMAP` |
| 小文件 | `CopyFile2` | `copy_file_range` |
| 预分配 | `SetFileValidData` / 稀疏空洞 | `fallocate` |
| 大页 | `MEM_LARGE_PAGES` | `MAP_HUGETLB` |
| 状态 | 构建与全部测试通过 | Debian sid 真机全部通过 |
| 架构 | x86_64 / ARM64 | x86_64 / ARM64（系统调用号取自 `libc::SYS_*`） |
| 第三方依赖 | 无 | 仅 `libc` |

> Linux 侧已在真机验证：Debian sid、当期内核、btrfs，覆盖 `io_uring`、
> `copy_file_range`、`statx` 对齐探测、`FIEMAP` 与 `SIGINT` 中断。

## 测试

```bash
cargo test                 # 73 个单元 / 集成测试（Linux 上 74 个）
python tests/e2e.py        # 21 个端到端验收（用 hashlib 独立比对）
python tests/e2e.py append # 只跑名字匹配的用例
```

端到端测试刻意不信任被测程序：它用 Python 的 `hashlib` 重新计算整棵树的摘要。
覆盖空文件、4 KiB 边界、`1 MiB ± 1`、全零、文本、120 个小文件、追加、中段改写、
mtime、移动、删除、镜像、exclude、阻塞后端、无直接 I/O、稀疏源、物理顺序、
不同缓冲区大小等组合。

`cargo clippy --workspace --all-targets` 零告警，`cargo fmt --check` 是强制项。
lint 规则刻意很严（`clippy::all`、`pedantic`、`nursery`、`missing_docs` 全部
`deny`），每一处例外都写在 crate 根上并说明理由，见
[CONTRIBUTING.md](CONTRIBUTING.md)。

## 已知取舍

* **不做 CoW 文件系统（btrfs / XFS reflink）优化**：reflink 拷贝瞬时且免费，
  做了这个引擎在那些文件系统上就没意义了 —— 这是有意排除，不是遗漏。
* **不做"小文件打包成流"**：本地拷贝下那是双倍 I/O，改为在队列层聚合。
* **本地拷贝不做用户态压缩**：成本模型会算出它是负收益；只用 NTFS 透明压缩，
  且只在实测熵支持时才开。
* **ACL 需显式开启**：跨文件系统复制 DACL / POSIX ACL 失败率高，应该是决定
  而不是默认。
* **`--delta` 在同等速率的介质上不会启用**：这是模型在正常工作，不是缺陷。
* **`--index` 信任时间戳**：索引记录目标文件的块哈希，键是目标的 `(size, mtime)`。
  外部改写但时间戳没变时无法察觉，`--index=verify` 会多读一遍目标来校验。

## 许可

MIT OR Apache-2.0，二选一。
