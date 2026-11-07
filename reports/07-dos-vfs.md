# 07｜AmigaDOS、文件名解析、Handler 与 DOS Packet

## 现有系统机制

AmigaDOS 负责文件、目录、Shell/CLI 和 Process 的文件环境。DOS `Process` 是具有 DOS 运行环境的 Exec Task；并非天然拥有受保护虚拟地址空间的 Linux 进程。

核心区分：

- `Open/Read/Write/Close`：应用使用的高层 DOS API。
- `dos.library`：解析设备、卷、Assign，转换成 Handler 可理解的消息协议。
- **Handler / 文件系统进程**：处理 DOS Packet。
- **Exec Device**：可为底层块/串口硬件提供 I/O。
- `SYS:`、`RAM:`、`CON:`、`Work:`：DOS 名字空间里的卷或设备路径。
- `C:`、`LIBS:`、`S:` 等：可能由 Assign 映射至实际目录。

## 文件调用流程

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
    VFS inode/driver/backing store
          │
    reply packet and result
          ▼
     FileHandle → Read/Close
```

**细节注意**：经典 DOS Packet 的动作编码、BPTR/BCPL 指针和字段具有真实 ABI；上图只是概念流程，不能代替官方 Packet 结构定义。

## 建议 VFS 的公共对象

```c
typedef struct {
    uint64_t id, parent_id;
    unsigned type, mode;
    uint64_t size;
    uint64_t mtime_ns;
} VfsNode;

typedef struct {
    VfsNode *node;
    uint64_t offset;
    unsigned flags;
    void *filesystem_private;
} VfsHandle;
```

这些属于你自己的 VFS 内部结构，不是 AmigaDOS 公共 ABI。

## 路径解析要点

1. `name:` 首先解释为卷、设备或 Assign（不是 Unix drive-letter）。
2. 根据 DOS 语义处理相对路径、当前目录及父目录操作，切忌直接将 `/`/`..` 的 POSIX 规则套入兼容层。
3. 实现 `Lock()` / `UnLock()` / `DupLock()` 的资源生命周期。
4. 目录遍历必须处理迭代句柄与目录变化。
5. Assign 可能包含多个目标目录或延迟解析。
6. 文件系统可能不区分大小写；应由各文件系统实现具体比较规则。
7. 路径长度、字符编码和 `:` 等特殊字符的合法性应由名称空间层统一规定。

## Handler 最小 Packet 子集

优先实现：

| 动作 | 功能 |
| --- | --- |
| `ACTION_LOCATE_OBJECT` | 根据 Lock/名字查找对象 |
| `ACTION_FREE_LOCK` | 释放目录/文件 Lock |
| `ACTION_FINDINPUT` | 打开只读文件 |
| `ACTION_FINDOUTPUT` | 创建/打开输出文件 |
| `ACTION_READ` / `ACTION_WRITE` | 读写 |
| `ACTION_END` | 关闭文件句柄 |
| `ACTION_EXAMINE_OBJECT` | 查询对象元数据 |
| `ACTION_EXAMINE_NEXT` | 遍历目录 |

表述来自经典 AmigaDOS Packet 族的常见概念；正式开发要按**目标兼容层的 SDK**逐一核对动作名称、返回值和参数编码。

## 必须测试的情况

- `SYS:` / `RAM:` / `C:` 的映射及嵌套 Assign。
- 文件不存在、权限不足、设备未挂载、磁盘空间耗尽。
- 非法路径、过长路径、目录被并发删除。
- 释放 Lock 后不再允许使用旧句柄。
- 发送方异常退出时 Handler 不永久保留悬空请求。
- “从 GUI 启动”与“从 CLI 启动”都正确建立 DOS Process 环境。

## 参考

- [AmigaOS AmigaDOS Packets](https://wiki.amigaos.net/wiki/AmigaDOS_Packets)
- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS dos.library Guide](https://developers.aros.org/documentation/app-dev/dos-library.html)
