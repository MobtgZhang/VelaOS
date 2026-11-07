# 08｜块设备、分区、文件系统与持久化

## 现有系统机制

AmigaOS 的文件系统通常由 DOS 文件系统/Handler 暴露给应用。经典 Amiga 原生磁盘常见 FFS（Fast File System）等；现代 AmigaOS/AROS 还可能提供 FAT、SFS、PFS3 等选择，**具体可读写范围随版本、驱动和发行版不同**。

AROS 2026 年官方开发动态提及 AFS、FAT、SFS 与 PFS3 的维护、64 位修复和其他增强；不能据此假定每个发布镜像都有全部功能。

## 存储堆栈

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

RDB 是经典 Amiga 硬盘分区环境的重要格式；若以 PC 原生启动作为首版目标，先实现 GPT/MBR 与虚拟块设备更容易。对外 DOS 路径仍可采用 `SYS:`、`DH0:` 语义。

## 推荐实现顺序

### Phase S0：只读 initrd

- 启动器把初始文件映像传入 RAM。
- 提供 `SYS:` 卷、只读 `Open/Read/Seek/Examine`。
- 应用程序及启动脚本先从这里加载。
- **目标**：彻底规避早期磁盘写损坏。

### Phase S1：可写 RAMFS

- 目录树、文件句柄、动态数据块、改名/删除/新建。
- 支持读写顺序/随机访问。
- 支持文件锁、目录枚举和时间戳。
- 用于自动化测试。

### Phase S2：块设备

- `BlockDevice` 定义 `read_blocks`、`write_blocks`、`flush`、`capacity`。
- QEMU 的虚拟设备优先。
- 加入缓冲区缓存、写回顺序和 I/O 错误传播。
- 只有 flush 成功后才向上层宣称关键数据持久化。

### Phase S3：FAT 文件系统

- 可优先采用成熟、授权许可兼容的 FAT 实现，或自行写教学版。
- 正确处理簇链、目录项、文件大小更新以及崩溃恢复边界。
- FAT 通常不具备现代强事务一致性，不应夸大数据安全性。

### Phase S4：Amiga 兼容文件系统

- 根据真实目标选择 FFS/SFS/PFS3 与相应 Disk/RDB 兼容。
- 分开验证大端元数据、校验和、国际化文件名、权限位。
- 文件系统格式实现应以公开规格及可信参考代码为依据。

## 内部接口（自拟）

```c
struct BlockDeviceOps {
    int (*read)(uint64_t lba, void *buf, uint32_t blocks);
    int (*write)(uint64_t lba, const void *buf, uint32_t blocks);
    int (*flush)(void);
};
struct FileSystemOps {
    int (*mount)(Volume *);
    int (*lookup)(Node *dir, const char *name, Node **result);
    int (*open)(Node *, OpenFlags, FileHandle *);
    int (*read)(FileHandle *, void *, size_t, size_t *);
    int (*write)(FileHandle *, const void *, size_t, size_t *);
    int (*readdir)(FileHandle *, DirEntry *);
    int (*sync)(Volume *);
};
```

这些只是新 OS 内部接口，**不是 AmigaOS 原生 API**。

## 数据一致性规则

- 将块 I/O 的超时、部分完成和错误传到文件系统，不可默认写入成功。
- 卸载之前等待所有在途请求完成。
- 热拔出时所有句柄应看到设备移除错误。
- 页缓存与块缓存同时存在时需定义单一真相源。
- 先实现只读的 fsck/一致性扫描器，再考虑修复工具。

## 验收

- 在 RAMFS 中执行 10 万次随机创建、写入、重命名、删除后结构一致。
- 使用 QEMU 虚拟磁盘、随机断电测试，不应损坏其他分区。
- 同文件并发读写策略明确（读写互斥或锁）。
- 一次写入失败不会误报全部成功。
- 设备拔出或文件系统卸载后旧句柄返回明确错误。

## 参考

- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS DOS API](https://developers.aros.org/documentation/app-dev/dos-library.html)
- [AROS 2026 Development News](https://aros.org/)
