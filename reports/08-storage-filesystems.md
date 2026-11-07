# 08｜块设备、分区、文件系统与持久化

本报告定义 VelaOS 块设备层、分区表、文件系统与持久化存储的完整设计，覆盖从 initrd 只读引导到可写 RAMFS、FAT、Amiga 原生文件系统的全栈路径。

---

## 1. 现有系统机制

AmigaOS 的文件系统通常由 DOS 文件系统/Handler 暴露给应用。经典 Amiga 原生磁盘常见 FFS（Fast File System）等；现代 AmigaOS/AROS 还可能提供 FAT、SFS、PFS3 等选择，**具体可读写范围随版本、驱动和发行版不同**。

AROS 2026 年官方开发动态提及 AFS、FAT、SFS 与 PFS3 的维护、64 位修复和其他增强；不能据此假定每个发布镜像都有全部功能。VelaOS 首版以内存文件系统（RAMFS）为主，FAT 作为可移植块设备文件系统，FFS/SFS 作为后续 Amiga 兼容目标。

---

## 2. 存储堆栈

```text
dos.library
     │
filesystem Handler (RAMFS / FAT / FFS-compatible ...)
     │
buffer cache / block allocator / metadata journal (optional)
     │
block.device abstraction
     │
partition manager (RDB / MBR / GPT)
     │
virtio-blk / AHCI / NVMe / removable media driver
     │
Disk hardware
```

RDB（Rigid Disk Block）是经典 Amiga 硬盘分区环境的重要格式；若以 PC 原生启动作为首版目标，先实现 GPT/MBR 与虚拟块设备更容易。对外 DOS 路径仍可采用 `SYS:`、`DH0:` 语义。

---

## 3. 推荐实现顺序

### Phase S0：只读 initrd

- 启动器（UEFI loader）把初始文件映像（initrd）传入 RAM。
- 内核把 initrd 挂载为只读 `SYS:` 卷。
- 提供只读 `Open/Read/Seek/Examine/ExamineNext`。
- 应用程序及启动脚本先从这里加载。
- **目标**：彻底规避早期磁盘写损坏，确保启动可重复。

**接口：**

```c
struct InitrdFS {
    const uint8_t *image;
    uint64_t size;
    /* 解析出的目录项表，只读 */
    InitrdEntry *entries;
    uint32_t entry_count;
};
int initrd_open(InitrdFS *fs, const char *path, InitrdFile *out);
int initrd_read(InitrdFile *f, void *buf, uint64_t len, uint64_t *out_n);
int initrd_seek(InitrdFile *f, int64_t off, int whence);
int initrd_examine(InitrdFS *fs, const char *path, InitrdStat *st);
int initrd_readdir(InitrdFS *fs, const char *dir, InitrdEntry *out, uint32_t max);
```

### Phase S1：可写 RAMFS

- 目录树、文件句柄、动态数据块、改名/删除/新建。
- 支持读写顺序/随机访问（`Seek`）。
- 支持文件锁、目录枚举和时间戳（monotonic ns）。
- 用于自动化测试、临时文件、Shell 工作目录。
- **容量上限**：由可用内存决定，可配置最大页数，超限返回 `NO_MEMORY`。

**内部接口：**

```c
struct RamNode {
    uint64_t id;
    uint64_t parent_id;
    uint32_t type;      /* RAM_TYPE_FILE / RAM_TYPE_DIR */
    uint32_t mode;      /* 保护位 */
    uint64_t size;
    uint64_t mtime_ns;
    char name[108];
    uint8_t *data;      /* 文件数据，目录为 NULL */
    uint64_t data_cap;  /* 已分配容量 */
    RamNode *first_child;
    RamNode *next_sibling;
};

int ramfs_create(RamNode *parent, const char *name, uint32_t type, RamNode **out);
int ramfs_write(RamNode *node, uint64_t offset, const void *buf, uint64_t len);
int ramfs_read(RamNode *node, uint64_t offset, void *buf, uint64_t len, uint64_t *out_n);
int ramfs_unlink(RamNode *parent, const char *name);
int ramfs_rename(RamNode *old_parent, const char *old_name,
                 RamNode *new_parent, const char *new_name);
int ramfs_readdir(RamNode *dir, uint64_t *cookie, RamDirEntry *out, uint32_t max);
```

### Phase S2：块设备

- `BlockDevice` 定义 `read_blocks`、`write_blocks`、`flush`、`capacity`。
- QEMU 的虚拟设备（virtio-blk）优先，随后是 AHCI/SATA 和 NVMe。
- 加入缓冲区缓存、写回顺序和 I/O 错误传播。
- 只有 `flush` 成功后才向上层宣称关键数据持久化。
- 分区表支持：MBR（第1扇区）、GPT（LBA 1-33）、RDB（Amiga 经典）。

**接口：**

```c
struct BlockDeviceOps {
    int (*read)(uint64_t lba, void *buf, uint32_t blocks);    /* 块大小 512 */
    int (*write)(uint64_t lba, const void *buf, uint32_t blocks);
    int (*flush)(void);
    uint64_t (*capacity)(void);                               /* 返回块数 */
    uint32_t (*block_size)(void);
};

struct Partition {
    uint64_t start_lba;
    uint64_t block_count;
    uint32_t type;      /* PART_TYPE_FAT / PART_TYPE_FFS / PART_TYPE_UNKNOWN */
};

int block_register(BlockDeviceOps *ops, const char *name);
int block_partition_scan(const char *dev_name, Partition *out, uint32_t max);
```

### Phase S3：FAT 文件系统

- 可优先采用成熟、授权许可兼容的 FAT 实现（如 FATFS），或自行写教学版。
- 正确处理簇链、目录项、文件大小更新、根目录、FAT 表。
- 支持 FAT12/FAT16/FAT32。
- FAT 通常不具备现代强事务一致性，不应夸大数据安全性；断电可能导致元数据不一致。
- 只读挂载选项，避免意外写入损坏。

**关键结构：**

```c
struct FatBS {
    uint16_t bytes_per_sector;
    uint8_t  sectors_per_cluster;
    uint16_t reserved_sectors;
    uint8_t  num_fats;
    uint16_t root_entry_count;       /* FAT12/16 */
    uint16_t total_sectors16;
    uint32_t total_sectors32;
    uint32_t sectors_per_fat;        /* FAT32 */
    uint32_t root_cluster;           /* FAT32 */
};

struct FatFS {
    FatBS bs;
    uint64_t fat_lba;
    uint64_t data_lba;
    uint64_t root_dir_lba;
    uint8_t *fat_cache;              /* FAT 表缓存 */
};
```

### Phase S4：Amiga 兼容文件系统

- 根据真实目标选择 FFS/SFS/PFS3 与相应 Disk/RDB 兼容。
- 分开验证大端元数据、校验和、国际化文件名、权限位。
- 文件系统格式实现应以公开规格及可信参考代码为依据。
- 启动卷（Bootable）需支持 RDB 引导块和文件系统头块。

---

## 4. 内部接口（自拟）

```c
struct FileSystemOps {
    int (*mount)(Volume *vol, void **fs_private);
    int (*unmount)(void *fs_private);
    int (*lookup)(void *fs_private, VfsNode *dir, const char *name, VfsNode **result);
    int (*open)(void *fs_private, VfsNode *node, uint32_t flags, VfsHandle **out);
    int (*read)(void *fs_private, VfsHandle *fh, void *buf, size_t len, size_t *out_n);
    int (*write)(void *fs_private, VfsHandle *fh, const void *buf, size_t len, size_t *out_n);
    int (*seek)(void *fs_private, VfsHandle *fh, int64_t off, int whence, uint64_t *out_pos);
    int (*readdir)(void *fs_private, VfsHandle *fh, VfsDirEntry *out, uint32_t max);
    int (*create)(void *fs_private, VfsNode *parent, const char *name, uint32_t type, VfsNode **out);
    int (*unlink)(void *fs_private, VfsNode *parent, const char *name);
    int (*rename)(void *fs_private, VfsNode *old_p, const char *old_n,
                  VfsNode *new_p, const char *new_n);
    int (*stat)(void *fs_private, VfsNode *node, VfsStat *st);
    int (*sync)(void *fs_private);
};
```

这些只是新 OS 内部接口，**不是 AmigaOS 原生 API**。

---

## 5. 缓冲区缓存（Block Cache）

- LRU 淘汰，固定页数上限（如 256 页）。
- 脏页标记，`flush`/`sync` 时回写。
- 读命中直接返回，未命中发起块读。
- 并发控制：每个缓存页独立锁，避免全局锁竞争。
- 失效：文件被截断或删除时，相关缓存页应被驱逐。

---

## 6. 数据一致性规则

- 将块 I/O 的超时、部分完成和错误传到文件系统，不可默认写入成功。
- 卸载之前等待所有在途请求完成，再刷新元数据。
- 热拔出时所有句柄应看到设备移除错误（`ERROR_DEVICE_NOT_FOUND`）。
- 页缓存与块缓存同时存在时需定义单一真相源：块设备缓存为真，文件系统页缓存为缓存。
- 先实现只读的 fsck/一致性扫描器，再考虑修复工具。
- RAMFS 无块设备，写入即时生效，但内存耗尽时返回错误。

---

## 7. 验收

- 在 RAMFS 中执行 10 万次随机创建、写入、重命名、删除后结构一致（无孤儿节点、无循环链表）。
- 使用 QEMU 虚拟磁盘、随机断电测试，不应损坏其他分区；FAT 卷在断电后应可挂载只读。
- 同文件并发读写策略明确：读写互斥（写独占锁，读共享锁）。
- 一次写入失败不会误报全部成功：部分写入返回实际字节数，后续写入返回错误。
- 设备拔出或文件系统卸载后旧句柄返回明确错误（`ERROR_OBJECT_NOT_FOUND`）。
- `INFO` 命令正确报告每个卷的空闲块和总块数。

---

## 8. 参考

- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS DOS API](https://developers.aros.org/documentation/app-dev/dos-library.html)
- [AROS 2026 Development News](https://aros.org/)
- [FAT32 Specification](https://www.microsoft.com/en-us/windows/windows-server/technical-specifications)
