# 07｜AmigaDOS、文件名解析、Handler 与 DOS Packet

本报告定义 VelaOS 中 AmigaDOS 兼容层的完整设计与实现规范，涵盖 DOS 调用流程、VFS 公共对象、路径解析、Handler  Packet 子集、Assign/卷管理、DOS Process 环境，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

AmigaDOS 负责文件、目录、Shell/CLI 和 Process 的文件环境。在 VelaOS 中，DOS `Process` 是具有 DOS 运行环境的 Exec Task；并非天然拥有受保护虚拟地址空间的 Linux 进程。

核心区分：

| 层级 | 职责 |
| --- | --- |
| `Open/Read/Write/Close` | 应用使用的高层 DOS API |
| `dos.library` | 解析设备、卷、Assign，转换成 Handler 可理解的消息协议 |
| **Handler / 文件系统进程** | 处理 DOS Packet，持有 VFS 节点与数据块 |
| **Exec Device** | 为底层块/串口硬件提供 I/O（ATA/NVMe/SATA/ramfs） |
| `SYS:`、`RAM:`、`CON:`、`Work:` | DOS 名字空间里的卷或设备路径 |
| `C:`、`LIBS:`、`S:` 等 | 由 Assign 映射至实际目录 |

---

## 2. DOS 调用流程

```text
App: Open("SYS:readme.txt")
           │
      dos.library
           │ resolve SYS: / Assign / Lock
           ▼
       DosEntry / volume
           │
    ACTION_FINDINPUT packet
           │ PutMsg
           ▼
  FileSystem Handler task
           │
     VFS inode / driver / backing store
           │
     reply packet and result
           ▼
     FileHandle → Read/Close
```

每一步都由 syscall 层转发。应用通过 `vela_sys::api::open` 进入 exec syscall，由 `dos.library` 处理器解析路径，发送 DOS Packet 给目标 Handler，Handler 执行后回复，结果经 `dos.library` 返回给应用。

**细节注意**：经典 DOS Packet 的动作编码、BPTR/BCPL 指针和字段具有真实 ABI；下文只给出概念流程，正式开发要按目标兼容层的 SDK 逐一核对动作名称、返回值和参数编码。

---

## 3. VFS 公共对象（内部接口）

这些属于 VelaOS 自己的 VFS 内部结构，不是 AmigaOS 公共 ABI。

```c
typedef struct {
    uint64_t id;           /* 全局唯一节点 id */
    uint64_t parent_id;    /* 父目录 id，根为 0 */
    uint32_t type;         /* VFS_TYPE_FILE / VFS_TYPE_DIR / VFS_TYPE_DEV */
    uint32_t mode;         /* 保护位：可读/可写/可执行 */
    uint64_t size;         /* 文件大小（字节） */
    uint64_t mtime_ns;     /* 修改时间（单调纳秒） */
    char name[108];        /* 节点名称，NUL 终止 */
} VfsNode;

typedef struct {
    VfsNode *node;         /* 挂载的节点 */
    uint64_t offset;       /* 当前读写偏移 */
    uint32_t flags;        /* O_READ / O_WRITE / O_APPEND */
    void *fs_private;      /* 文件系统私有数据（如簇号、块偏移） */
} VfsHandle;

typedef struct {
    uint64_t volume_id;    /* 卷 id */
    char name[32];         /* 卷名，如 "RAM", "SYS", "DH0" */
    uint32_t flags;        /* VOL_READ_ONLY / VOL_REMOVABLE */
    void *fs_private;      /* 文件系统私有数据 */
} Volume;
```

VFS 层提供以下原语，供 Handler 和 DOS 层调用：

| 原语 | 含义 |
| --- | --- |
| `vfs_lookup(volume, path)` | 解析路径，返回 VfsNode* |
| `vfs_open(node, flags)` | 分配 VfsHandle，返回句柄 |
| `vfs_read(handle, buf, len)` | 从偏移处读取，返回实际字节数 |
| `vfs_write(handle, buf, len)` | 写入并更新偏移 |
| `vfs_close(handle)` | 释放句柄 |
| `vfs_readdir(handle, entries, max)` | 返回目录项数组 |
| `vfs_create(parent, name, type)` | 新建文件/目录节点 |
| `vfs_unlink(parent, name)` | 删除节点 |
| `vfs_rename(old_parent, old_name, new_parent, new_name)` | 重命名/移动 |
| `vfs_stat(node, stat_buf)` | 填充元数据 |

---

## 4. 路径解析要点

1. `name:` 首先解释为卷、设备或 Assign（不是 Unix drive-letter）。
2. 根据 DOS 语义处理相对路径、当前目录及父目录操作，切忌直接将 `/`/`..` 的 POSIX 规则套入兼容层。
3. 实现 `Lock()` / `UnLock()` / `DupLock()` 的资源生命周期：Lock 引用计数加 1，UnLock 减 1，归零时释放。
4. 目录遍历必须处理迭代句柄与目录变化：遍历时若目录被修改，后续条目可能失效，需返回明确错误。
5. Assign 可能包含多个目标目录或延迟解析：`ASSIGN C: SYS:C LIBS:C` 表示按顺序查找。
6. 文件系统可能不区分大小写：应由各文件系统实现具体比较规则，VFS 层不做假设。
7. 路径长度、字符编码和 `:` 等特殊字符的合法性应由名称空间层统一规定：单个路径分量上限 107 字节（与 DirEntry 对齐），总路径上限 1024 字节。

### 路径解析算法伪代码

```c
VfsNode *resolve_path(const char *path, Volume *cwd_vol, VfsNode *cwd_node) {
    Volume *vol;
    const char *rest;
    split_volume_prefix(path, &vol, &rest);   /* 提取 "SYS:" 等 */
    if (!vol) vol = cwd_vol;
    VfsNode *node = vol ? vol->root : cwd_node;
    while (*rest) {
        rest = skip_separators(rest);
        const char *comp = next_component(rest, &rest);
        if (strcmp(comp, ".") == 0) continue;
        if (strcmp(comp, "..") == 0) {
            node = node->parent_id ? vfs_get_node(node->parent_id) : node;
            continue;
        }
        node = vfs_lookup_child(node, comp);
        if (!node) return NULL;               /* PATH_NOT_FOUND */
    }
    return node;
}
```

---

## 5. Handler 最小 Packet 子集

优先实现以下 DOS Packet 动作，其余动作先返回 `ERROR_ACTION_NOT_KNOWN`：

| 动作 | 功能 | 参数（概略） |
| --- | --- | --- |
| `ACTION_LOCATE_OBJECT` | 根据 Lock/名字查找对象 | name, lock, mode |
| `ACTION_FREE_LOCK` | 释放目录/文件 Lock | lock |
| `ACTION_FINDINPUT` | 打开只读文件 | name, mode |
| `ACTION_FINDOUTPUT` | 创建/打开输出文件 | name, mode |
| `ACTION_READ` / `ACTION_WRITE` | 读写 | handle, buf, len |
| `ACTION_END` | 关闭文件句柄 | handle |
| `ACTION_EXAMINE_OBJECT` | 查询对象元数据 | lock/name, info_buf |
| `ACTION_EXAMINE_NEXT` | 遍历目录 | handle, info_buf |
| `ACTION_CREATE_DIR` | 新建目录 | parent_lock, name |
| `ACTION_DELETE_OBJECT` | 删除文件/目录 | lock/name |
| `ACTION_RENAME_OBJECT` | 重命名/移动 | old_lock, old_name, new_lock, new_name |
| `ACTION_FINDUPDATE` | 以读写方式打开 | name, mode |
| `ACTION_FLUSH` | 刷新缓存 | handle |
| `ACTION_INFO` | 卷空闲/总空间 | lock, info_buf |
| `ACTION_SET_PROTECT` | 设置保护位 | lock/name, mask |

Packet 分发由 Handler 任务的消息循环完成：`GetMsg` 取包、根据 `dp_Action` 分派、执行后 `ReplyMsg` 返回结果。每个 Packet 带有回复端口，确保调用方可等待结果。

---

## 6. Assign 与卷管理

| 卷名 | 含义 | 典型 backing |
| --- | --- | --- |
| `SYS:` | 系统卷，包含启动文件、命令、库 | initrd / RAMFS |
| `RAM:` | 可写内存文件系统 | RAMFS |
| `C:` | 命令目录，通常 Assign 到 `SYS:C` | Assign |
| `LIBS:` | 库目录，通常 Assign 到 `SYS:LIBS` | Assign |
| `S:` | 脚本目录，通常 Assign 到 `SYS:S` | Assign |
| `ENV:` | 当前会话环境变量 | 内存表 |
| `ENVARC:` | 持久环境变量 | RAMFS 文件 |
| `CON:` | 控制台设备 | 输入/输出流 |
| `NIL:` | 空设备，读返回 EOF，写丢弃 | 虚拟设备 |

Assign 语义：`ASSIGN NAME: TARGET1 TARGET2` 表示按顺序在各目标中查找；若只有一个目标且无 `ADD` 标志，则替换原有映射。`ASSIGN` 无参数时列出所有映射。

---

## 7. DOS Process 环境

每个 DOS Process 关联：

- 当前卷与当前目录（`CD` 的目标）
- 标准输入/输出/错误句柄（默认 `CON:`）
- 局部环境变量表（`ENV:`）
- 当前 Assign 表（可继承自父进程，也可独立修改）
- 返回码（`ReturnCode`，子进程退出时设置）

Shell 启动新程序时，应复制父进程的 DOS 环境，允许子进程独立修改而不影响父进程。

---

## 8. 必须测试的情况

- `SYS:` / `RAM:` / `C:` 的映射及嵌套 Assign。
- `ASSIGN C: SYS:C` 后能够从 Shell 执行命令。
- 文件不存在、权限不足、设备未挂载、磁盘空间耗尽。
- 非法路径、过长路径（>1024 字节）、目录被并发删除。
- 释放 Lock 后不再允许使用旧句柄。
- 发送方异常退出时 Handler 不永久保留悬空请求。
- “从 GUI 启动”与“从 CLI 启动”都正确建立 DOS Process 环境。
- `INFO` 命令正确报告卷空闲空间和总容量。
- `ExamineNext` 遍历大目录不丢条目、不重复。

---

## 9. 参考

- [AmigaOS AmigaDOS Packets](https://wiki.amigaos.net/wiki/AmigaDOS_Packets)
- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS dos.library Guide](https://developers.aros.org/documentation/app-dev/dos-library.html)
- [AmigaDOS Reference Manual](https://wiki.amigaos.net/wiki/AmigaDOS_Reference_Manual)
