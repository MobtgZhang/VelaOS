# 03｜Exec 对象、动态库、Resource 与 Resident

## 现有系统机制

Exec 是 AmigaOS 的核心管理者，负责 Task、内存、消息端口、Device、Resource、Library 等。经典库由 `Library` 结构、函数向量表与私有数据组成，`OpenLibrary()` / `CloseLibrary()` 维护使用计数。

- **AmigaOS 3.x**：库的函数向量表与稳定入口偏移（经典 LVO）是 ABI 的一部分。
- **AmigaOS 4.x**：多数现代库还使用 `GetInterface()`、`DropInterface()` 的接口模型，不能把 3.x 库调用写法直接视为 4.x 的全部规则。
- **AROS**：重实现类似 Exec 库/设备/资源管理，模块构建系统支持 `library`、`device`、`resource`、`hidd`、`handler`、`datatype`、`gadget` 等类型。

## 对象分类

| 对象 | 用途 | 使用方式 |
| --- | --- | --- |
| `Library` | API 函数与内部状态 | Open/Close，函数向量或 Interface |
| `Device` | 有 I/O 请求生命周期的驱动 | OpenDevice/SendIO/DoIO/CloseDevice |
| `Resource` | 低级硬件共享/仲裁入口 | OpenResource，通常没有 CloseResource |
| `Resident` | 模块的识别/自动初始化描述 | 启动扫描或按需装载 |
| `Task` | 执行上下文 | 调度器创建/删除 |
| `MsgPort` | Task 间消息入口 | PutMsg/GetMsg/ReplyMsg |

`Resource` 不等同于 `Device`：资源一般服务低层硬件协调，应用首选设备和库。

## 核心数据结构（设计伪代码）

```c
struct Module {
    char name[64];
    uint16_t version, revision;
    enum { MOD_LIBRARY, MOD_DEVICE, MOD_RESOURCE } kind;
    atomic_uint open_count;
    bool initialized, expunge_pending;
    void *base;
    void **vectors;
    Mutex lifecycle_lock;
};
```

**不要将上面布局当成 Amiga ABI。** 真正兼容时需使用目标系统公开头文件规定的 `Library`、`Node`、LVO、对齐与调用约定。

## 库加载与卸载时序

```text
OpenLibrary("foo.library", minVersion)
  │ 查找已注册库
  ├─ 命中：校验版本、open_count++
  └─ 未命中：解析 Resident/模块 → 装载 → 初始化 → 注册
                            ↓
                         返回 Base
                            ↓
CloseLibrary(Base) → open_count-- → 条件满足时 Expunge
```

设计注意：

- 多个 Task 同时 `OpenLibrary` 时，初始化不能重复发生。
- 失败的初始化必须回滚临时注册与资源。
- 有正在调用的函数时不可释放其代码页。
- `CloseLibrary` 与异步回调的完成顺序要明确。
- `Expunge` 可以延迟，不能仅靠 open_count 立即卸载。

## 公开 ABI 规则

1. 版本变化新增向量，不改变旧向量意义和偏移。
2. 先设计 `Open/Close/Expunge/Reserved` 生命周期，后加入模块业务函数。
3. 系统公开结构体与私有内部结构分开。
4. `TagItem`、`Hook` 与 BOOPSI 对象应由统一 utility 机制解释。
5. AROS x86-64 代码使用 `IPTR` 类可容纳指针的类型，注意经典 `BPTR` 语义。
6. 兼容原生 Amiga 时重新审查 BCPL 指针换算、大小端与 ABI。

## 最小目标与测试

- 注册 `hello.library`，两个 Task 可同时打开、调用、关闭。
- 版本要求过高时必须失败。
- 库的第一次初始化只执行一次。
- 初始化失败后再次装载不会看到脏状态。
- 不能在未完成异步 I/O 时销毁对应设备代码。

## 参考

- [AmigaOS Exec Libraries](https://wiki.amigaos.net/wiki/Exec_Libraries)
- [AmigaOS Exec Resources](https://wiki.amigaos.net/wiki/Exec_Resources)
- [AROS Module Types](https://developers.aros.org/documentation/sys-dev/modules.html)
- [AROS Exec Library](https://developers.aros.org/documentation/app-dev/exec-library.html)
