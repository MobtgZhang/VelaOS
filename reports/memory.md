# AmigaOS 与 AROS 内存管理详解

> **适用范围**：经典 AmigaOS 3.x、AmigaOS 4.x（ExecSG）及 AROS 的常见原生/宿主构建。  
> **资料核对日期**：2026-11-06  
> **注意**：这三类系统在内存管理上有显著区别；API 的存在不代表所有平台都提供相同的硬件内存保护能力。

## 1. 摘要

AmigaOS 和 AROS 均使用 **Exec** 体系管理内存。程序通过系统 API 申请内存、使用内存并负责释放内存，不应把固定物理地址当成普通应用可依赖的接口。

- **经典 AmigaOS 3.x**：以全局共享地址空间、内存区域链表和 `AllocMem()` / `AllocVec()` 为主；通常没有进程级内存隔离。
- **AmigaOS 4.x**：ExecSG 使用 MMU、虚拟地址映射、物理页分配及更现代的分配器，提供有限的访问保护；仍未实现与现代 Linux/Windows 完全相同的、普遍强制的进程隔离模型。
- **AROS**：保留 Exec 风格 API 和内存池机制；是否使用 MMU、能够进行何种内存映射和保护，取决于 CPU 架构、构建目标和移植实现。不能把旧版文档中的设计构想直接当作所有发行版的现有特性。

**术语区别**：

| 概念 | 含义 |
| --- | --- |
| 内存分配 | 从系统管理的空闲内存中取得一块可用区域 |
| 物理内存 | 实际 RAM 芯片上的地址和页 |
| 虚拟内存 | CPU 访问的虚拟地址经 MMU 映射到物理地址 |
| 内存保护 | 限制对地址范围的读取、写入或执行 |
| 进程隔离 | 一个进程不能随意读写另一个进程的内存 |
| 共享内存 | 多个任务依据约定访问同一内存区域 |
| 内存池 | 从较大的分配区域中再分配较小对象，减少管理开销 |

> **重要**：有 MMU ≠ 有完整进程隔离；有 `MEMF_PRIVATE` API ≠ 可以假设所有任务都享有强制的私有地址空间。

## 2. Exec 在内存管理中的作用

在经典 Amiga 系统中，Exec 负责维护可用内存区域、分配内存块，并提供查询、释放和内存池接口：

```text
                    应用程序 / 系统服务
                            │
                 AllocVec / AllocMem
                            │
                         Exec
                            │
                  可用内存区域及分配器
                            │
             ┌──────────────┴─────────────┐
             │                            │
           Chip RAM                     Fast RAM
      （经典 Amiga 硬件）          （经典 Amiga 扩展内存）
```

上图适用于理解**经典 Amiga 硬件**。AROS 的 PC/ARM 移植不一定具有真实的 Chip RAM / Fast RAM 硬件区分；兼容性标志和物理硬件属性不可直接画等号。

## 3. 经典 AmigaOS 3.x

### 3.1 全局共享地址空间

经典 AmigaOS 的 Task 通常共同访问一个全局地址空间，Exec 不默认为每个 Task 建立类似 Linux 用户进程那样的独立虚拟地址空间。

这意味着：

1. 任务之间传递有效指针相对直接。
2. 消息队列中可以传递指向共享数据结构的指针。
3. 如果任务使用了悬空指针、越界地址或错误的内存释放操作，可能破坏其他任务或系统。
4. 某些 68020/68030/68040 等硬件可以具备 MMU，但**经典 AmigaOS 3.x 的标准 Exec 内存模型并不因此自动拥有完整的进程级保护**。

### 3.2 Chip RAM 与 Fast RAM

| 类别 | 典型用途 |
| --- | --- |
| `MEMF_CHIP` | 经典 Amiga 自定义芯片可 DMA 访问的内存，图形/音频等旧硬件场景使用 |
| `MEMF_FAST` | 通常由 CPU 使用，不供经典自定义芯片直接访问的较快扩展内存 |
| `MEMF_PUBLIC` | 声明内存预期可供其他任务使用；不等于现代意义的安全访问控制 |
| `MEMF_CLEAR` | 申请时清零 |
| `MEMF_ANY` | 不要求特定内存类别，由系统选择 |

不要把 Chip RAM / Fast RAM 当成所有 AROS 平台上都存在的两块真实内存。

### 3.3 常用内存 API

| API | 作用 | 正确的释放方式 |
| --- | --- | --- |
| `AllocMem(size, flags)` | 申请指定大小和属性的内存 | `FreeMem(ptr, size)` |
| `AllocVec(size, flags)` | 分配并记录其大小 | `FreeVec(ptr)` |
| `AvailMem(flags)` | 查询空闲内存或最大连续块 | 无 |
| `TypeOfMem(ptr)` | 查询地址所在内存类型 | 无 |
| `CreatePool(...)` | 建立内存池 | `DeletePool(pool)` |
| `AllocPooled(pool, size)` | 从池内分配 | `FreePooled(pool, ptr, size)` 或删除整个池 |

**经典 AmigaOS / AROS 风格 C 示例：**

```c
#include <exec/types.h>
#include <exec/memory.h>
#include <proto/exec.h>

void example(void)
{
    APTR buffer = AllocVec(4096, MEMF_PUBLIC | MEMF_CLEAR);

    if (buffer == NULL) {
        /* 内存不足：执行错误处理 */
        return;
    }

    /* 使用 buffer 指向的 4096 字节内存 */

    FreeVec(buffer);
}
```

这是 Exec API 的示意，具体编译方式依 SDK / 目标版本而定。

### 3.4 内存碎片与生命周期

经典分配器依赖内存块记录与空闲区域管理，持续分配/释放大小不一的内存会造成**外部碎片**。因此：

- 不要假定“总空闲内存足够”就一定能得到相同大小的**连续块**。
- 小对象分配较多时可考虑内存池。
- 资源结束使用后尽快释放。
- `AllocMem()` 与 `FreeMem()` 的大小应配套；`AllocVec()` 与 `FreeVec()` 必须配套。
- 不得访问已释放内存，也不得重复释放。

`AvailMem(MEMF_LARGEST)` 可帮助了解特定内存类别的最大可用块，但结果只代表查询时刻。

## 4. AmigaOS 4.x（ExecSG）

### 4.1 内存虚拟化

AmigaOS 4.x 的内存系统较经典版本有明显改变：

```text
                       应用程序
                           │
                AllocVecTags(请求参数)
                           │
                       ExecSG
             ┌─────────────┴─────────────┐
             │                           │
         虚拟地址管理                 内存分配器
             │                 （对象缓存/Slab 等）
             │                           │
             └─────────────┬─────────────┘
                           │
                        MMU 映射
                           │
                物理页管理 / Buddy
                           │
                          RAM
```

官方文档描述了基于 MMU 的虚拟地址映射、物理页的 Buddy 分配方法、用于小型对象和缓存的 Slab 分配机制，以及内存池。

**注意**：虚拟内存机制不意味着一定开启磁盘交换（swap），也不意味着所有运行版本具备完全隔离的用户进程地址空间。

### 4.2 推荐接口：AllocVecTags

AmigaOS 4.x 新代码优先使用 `AllocVecTags()`，不建议继续依赖已经标注为旧式的 `AllocMem()` / `AllocVec()` 作为新式分配的首选。

```c
#include <exec/types.h>
#include <exec/exectags.h>
#include <proto/exec.h>

/* AmigaOS 4.x SDK 风格示例 */
APTR p = IExec->AllocVecTags(
    4096,
    AVT_Type, MEMF_PRIVATE,
    AVT_ClearWithValue, 0,
    TAG_END
);

if (p != NULL) {
    /* 使用内存 */
    IExec->FreeVec(p);
}
```

上面是 **AmigaOS 4.x 专用** 的 API 使用示意，不应未经适配直接用于经典 3.x 或 AROS。

| 属性 | 作用 |
| --- | --- |
| `MEMF_PRIVATE` | 任务私有分配语义，4.x 新接口的常用默认类型 |
| `MEMF_SHARED` | 供多个 Task 共享的内存 |
| `MEMF_EXECUTABLE` | 存放可执行机器码的内存 |
| `AVT_Contiguous` | 请求物理连续区域；适合部分 DMA 场景 |
| `AVT_Lock` | 控制页是否锁定，避免被移动或换出（依具体接口语义） |

`MEMF_PRIVATE` 与 `MEMF_SHARED` 的选择必须符合真实用途。共享对象、消息数据或跨任务数据不能误配为不可共享区域。

### 4.3 MMU 与安全边界

AmigaOS 4.x 引入了基础的内存属性控制及保护，但为保持 Amiga 共享内存编程模型的兼容性，无法简单等同于现代操作系统的完整进程隔离。应避免以下推断：

- “有 MMU，所以每个任务默认完全隔离”——**不成立**。
- “申请私有内存就绝不会被其他任何代码破坏”——**不能作为普遍保证**。
- “Slab 分配器为 SMP 做了优化，所以正式系统已经支持 SMP”——**不成立**（内存分配器的潜在可扩展性与调度器的实际多核支持不同）。

### 4.4 DMA、缓存与页面锁定

硬件 DMA 设备通常使用物理总线地址，程序持有的却可能是虚拟地址。涉及驱动开发时，应按目标平台的 DMA API 和缓存一致性规则操作，而不是把虚拟指针直接交给设备。

锁定内存页会限制页迁移/换出；锁与解锁应严格成对。并非所有已分配内存都适合被长期锁住。

## 5. AROS 内存管理

### 5.1 兼容经典 API

AROS 的 Exec 实现提供类似以下接口：

```text
AllocMem / FreeMem
AllocVec / FreeVec
AvailMem / TypeOfMem
CreatePool / AllocPooled / FreePooled / DeletePool
AddMemList（驱动或平台内存注册场景）
```

常见内存标志包括 `MEMF_CLEAR`、`MEMF_PUBLIC`、`MEMF_CHIP`、`MEMF_FAST` 等；其中部分属性用于兼容 Amiga 语义，并非对所有现代硬件都有相同的物理含义。

### 5.2 内存池

```text
系统内存
   │
   └── CreatePool()
          │
          ├── AllocPooled() → 对象 A
          ├── AllocPooled() → 对象 B
          └── AllocPooled() → 对象 C

       FreePooled()：释放某一个对象
       DeletePool()：释放整个池及其中的剩余对象
```

在有大量小对象和共同生命周期的场景，内存池可减少分配开销。**共享内存池是否能够被并发访问，必须结合实现和所用锁确认**，不能直接假定所有版本都线程安全。

AROS 自动生成的 Exec API 文档还指出：在 SMP 构建中，部分底层 `MemHeader` 操作会通过内嵌自旋锁进行串行化；手工初始化相关结构时必须按照当前 SDK 要求初始化锁成员。

### 5.3 MMU、内存保护的实际边界

AROS 同时存在 m68k、x86、x86-64、ARM、AArch64 等移植，以及 hosted（运行在另一操作系统之上）和 native（原生）两类部署。

- 某些实现具备页映射、保护或其他 MMU 能力。
- 一些历史设计文档讨论了 `MEMF_PRIVATE`、只读共享等可能的保护方案。
- **历史设计提案不等于所有目标平台、所有发行版已经实现这些 API 或具有完整进程隔离**。
- hosted 版本依赖宿主操作系统运行，但这不表示 AROS 客体任务自动成为彼此完全隔离的宿主进程。

因此，开发时应核对目标 `AROS` 构建、SDK 版本、内存标志和实际移植文档，而不是把所有 AROS 一律视为“没有 MMU”或“已经完全内存保护”。

## 6. Task、Process、线程与内存的关系

在经典 AmigaOS/AROS 语境中：

- `Task` 是 Exec 调度的执行上下文，拥有自己的栈和运行状态。
- `Process` 是带有 DOS 环境的 Task，不应直接等同于 Linux 中具有独立受保护地址空间的进程。
- 多 Task 访问同一内存时需要同步与生命周期管理。
- 消息端口通常传递消息结构的指针，而不保证自动深拷贝消息内容。

```text
Task A ──┐
         ├── 共享内存 / 消息对象 ── 同步（信号量、互斥量或消息协议）
Task B ──┘
```

在 AmigaOS 4.x，跨任务共享内存时应按其 API 约定显式使用 `MEMF_SHARED`。

## 7. 开发时的检查清单

- [ ] 每次分配都检查返回指针是否为 `NULL`。
- [ ] 配对使用 `AllocMem/FreeMem`、`AllocVec/FreeVec`，不要混用。
- [ ] 不访问已释放的内存，不重复释放。
- [ ] 明确共享数据的所有权、访问权限、释放责任。
- [ ] 使用信号量/互斥量保护并发修改的数据结构。
- [ ] 对 DMA 缓冲区核对物理地址、页锁定和缓存规则。
- [ ] 在 SMP 构建中确认分配器及共享对象是否多核安全。
- [ ] 将“硬件 MMU 支持”“内存映射”“进程隔离”分别验证。
- [ ] 区分 AmigaOS 3.x、4.x 和 AROS 的 SDK/API，避免跨版本照搬。

## 8. 三套系统的速查对照

| 维度 | AmigaOS 3.x | AmigaOS 4.x | AROS |
| --- | --- | --- | --- |
| 核心内存接口 | AllocMem、AllocVec | 优先 AllocVecTags | AllocMem、AllocVec 等 |
| 传统共享地址空间 | 是 | 保留大量共享/兼容语义 | 大量继承 Amiga 语义 |
| MMU 虚拟内存 | 标准 Exec 不使用完整进程虚拟化 | 使用 MMU 虚拟映射 | 按移植与构建变化 |
| 完整默认进程隔离 | 否 | 不能视为完整实现 | 不能作统一保证 |
| 内存池 | 有 | 有，并增强实现 | 有 |
| 多核安全的分配器 | 非 SMP 目标 | 分配器有面向多核的设计 | SMP 构建正在/已经对关键路径加锁 |

## 9. 参考资料

1. [AmigaOS Wiki — Exec Memory Allocation](https://wiki.amigaos.net/wiki/Exec_Memory_Allocation)
2. [AmigaOS Wiki — Obsolete Exec Memory Allocation](https://wiki.amigaos.net/wiki/Obsolete_Exec_Memory_Allocation)
3. [AmigaOS Wiki — Programming AmigaOS 4: Exec — The Kernel](https://wiki.amigaos.net/wiki/Programming_AmigaOS_4%3A_Exec_-_The_Kernel)
4. [AROS — Exec Library Developer Documentation](https://developers.aros.org/documentation/app-dev/exec-library.html)
5. [AROS — Exec API Autodocs](https://developers.aros.org/documentation/autodocs/exec.html)
6. [AROS — FAQ](https://www.aros.org/documentation/faq.html)
7. [AROS — Historical design/ideas (non-authoritative for current features)](https://developers.aros.org/documentation/summaries/ideas.html)

---

**结论**：经典 AmigaOS 依赖高效率、强共享的内存模型；AmigaOS 4.x 用 MMU 与现代分配技术改善了这一模型；AROS 则延续 Amiga API，同时按不同平台逐步增强内存映射及多核安全性。内存保护与隔离程度必须按具体系统版本判断。
