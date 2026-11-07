# AmigaOS 与 AROS 多线程、多任务及 SMP 多核管理详解

> **适用范围**：经典 AmigaOS 3.x、AmigaOS 4.1/ExecSG、AROS 常规构建及实验性 SMP 移植。  
> **资料核对日期**：2026-11-06  
> **状态说明**：AROS SMP 和 AmigaOS 4 第三方替代内核都在演进中；不能将实验演示视为所有用户安装系统的统一能力。

## 1. 先说结论

**AmigaOS 从早期就有抢占式多任务，但经典 AmigaOS 和常规 AmigaOS 4.1 不等于成熟的 SMP 多核操作系统。AROS 则已经存在实际的实验性 SMP 移植。**

| 系统或构建 | 抢占式多任务 | 同一 CPU 上多 Task | 多 CPU 核心并行 | 说明 |
| --- | --- | --- | --- | --- |
| AmigaOS 3.x | 是 | 是 | 标准版本否 | 经典 Exec 单核调度 |
| AmigaOS 4.1 / ExecSG 正式系统 | 是 | 是 | 尚非成熟、普遍可用的 SMP | PowerPC 平台；多核硬件不代表各核心都用于系统调度 |
| AROS 常规单核构建 | 是 | 是 | 否 | 取决于所安装的构建 |
| AROS `pc-x86_64-smp` | 是 | 是 | 是（实验性） | 官网标记可运行、驱动支持不完整 |
| AROS ARM/AArch64 SMP 相关移植 | 是 | 是 | 已实现相关多核机制 | 仍须检查具体硬件、驱动和版本 |
| AmigaOS 4 第三方替代内核演示（2026） | 是 | 是 | 多核演示已报道 | 不等于 ExecSG 正式发行版已升级为全面 SMP |

## 2. 多任务、线程与 SMP 的区别

### 2.1 单核抢占式多任务

单核 CPU 在不同 Task 之间切换：

```text
时间轴 →
CPU0  | Task A | Task B | Task A | Task C | Task B | ...
```

从用户角度看多个程序同时工作，但**任一瞬间只有一个 Task 在该 CPU 上执行**（不考虑专用硬件/协处理器的异步工作）。

### 2.2 SMP（Symmetric Multiprocessing，对称多处理）

多个核心可以同时执行不同 Task：

```text
时间轴 →
CPU0  | Task A | Task A | Task C | Task D |
CPU1  | Task B | Task E | Task B | Task E |
CPU2  | Task F | Task F | Task A | Task F |
```

SMP 要求不仅能发现多个 CPU，还要有能够在多个 CPU 上安全调度 Task 的内核、同步机制和驱动支持。

### 2.3 Exec 的 Task 与 DOS Process

- **Task**：由 Exec 调度的基本执行单元，具备独立栈、保存的寄存器上下文、优先级、信号位和任务状态。
- **Process**：在 Task 之上附加 AmigaDOS 运行环境的执行对象，例如命令行相关环境。
- **Thread / 线程**：在一般 OS 术语中指执行流；在 Amiga/AROS 原生编程中，Task 是最接近的基本概念，但**不能简单假定 Exec Task 的所有语义与 POSIX pthread 完全一样**。

AROS 的部分运行时或移植也提供了 pthread 相关能力；是否可用及线程安全语义应按具体库和 SDK 核对。

## 3. Exec 的任务调度

### 3.1 基本状态

```text
        创建任务
           │
           ▼
        READY（就绪）
           │  被调度
           ▼
       RUNNING（运行）
        │           │
        │ 等待事件  │ 被抢占/让出 CPU
        ▼           ▼
   WAITING（等待） READY（就绪）
        │
        │ Signal / 消息 / I/O 完成
        └──────────► READY
```

实际内核中的状态常量和细节会因版本不同有所变化；以上是概念图。

### 3.2 基于优先级的抢占式调度

经典 Exec 以任务优先级及就绪/等待状态决定谁先使用 CPU。经典任务优先级通常是有符号的 `-128` 到 `+127`，普通应用经常使用 `0`。

特点：

- 就绪的高优先级 Task 会优先得到 CPU。
- 任务等待事件时可释放 CPU，让其他 Task 运行。
- 同优先级任务如何轮转由具体实现决定，不应假设所有版本都有同样的量子长度和策略。
- 不合理的高优先级繁忙循环可能让低优先级 Task 长时间得不到运行机会。

**抢占式调度 ≠ SMP**：单核系统也可以有高效的抢占式多任务。

### 3.3 常见任务相关 API

| API | 作用 |
| --- | --- |
| `CreateTask()` / `CreateTaskTags()` | 创建任务（具体可用形式依系统/SDK 而定） |
| `FindTask()` | 查找 Task，`FindTask(NULL)` 常用于获取当前 Task |
| `SetTaskPri()` | 设置任务优先级 |
| `RemTask()` / `DeleteTask()` | 移除任务或清理通过对应方法创建的 Task |
| `Wait()` | 等待指定信号 |
| `Signal()` | 通知某个 Task |
| `PutMsg()` / `GetMsg()` / `ReplyMsg()` | 基于消息端口进行任务通信 |

AROS API 文档对手工 `AddTask()` 还有特定的兼容性与弃用说明；应优先采用目标版本推荐的高级任务创建 API。

## 4. 任务之间如何通信和同步

### 4.1 Signal：轻量事件通知

Exec 信号通过位标志表示事件发生。经典模型每个 Task 有一组信号位，其中一部分保留给系统。

```text
Task A                       Task B
  │                            │
  │  Signal(Task B, mask)      │
  ├───────────────────────────►│
  │                            └─ Wait(mask) 返回
```

信号适合“有事件发生”的通知，不等于发送包含任意数据的消息。

### 4.2 Message Port：消息传递

```text
发送方 Task A              接收方 Task B
    │                          │
    ├── PutMsg(port, msg) ────►│
    │                          ├── GetMsg(port)
    │                          ├── 读取/处理消息
    │◄────── ReplyMsg(msg) ────┤
    │                          │
```

**重要内存约束**：Amiga 风格消息端口经常传递指向消息结构的指针，而不是在内核中自动复制消息数据。发送者应保证消息在接收者使用与应答完成前仍然有效；4.x 跨 Task 使用的数据需要满足共享内存约定。

### 4.3 Semaphore / Mutex：保护共享数据

当两个 Task 共同修改队列、计数器或链表时，必须协调访问。

- **SignalSemaphore**：经典 Exec 中广泛使用，可保护共享资源。
- **Mutex**：AmigaOS 4.x 提供专门的互斥量 API，通常更适合一般互斥场景。
- **Spinlock（自旋锁）**：SMP 内核的短临界区中常见，持锁竞争期间可能持续占用 CPU。

锁使用不当可能导致死锁、优先级反转或系统无响应。

## 5. 单核时代的 Forbid/Permit 为什么不能直接用于 SMP？

经典 Amiga 开发中存在：

- `Forbid()` / `Permit()`：禁止/恢复任务切换。
- `Disable()` / `Enable()`：禁止/恢复中断，属于非常敏感的低层操作。

在单核环境中，限制当前 CPU 的调度能够暂时防止**其他普通 Task**进入同一代码段，但这并不是可跨架构照搬的同步方案；调用 `Wait()` 等可能中断这种状态，且硬件中断也有单独规则。

在 SMP 环境里：

```text
CPU0: Forbid() → 当前核心暂停 Task 切换
CPU1: Task B   → 仍可并行修改共享数据
```

AROS 的 Exec API 文档明确提醒：在 `EXECSMP` 构建中，`Forbid()` 和 `Disable()` 的作用**仅限调用它们的 CPU**；跨核共享数据必须使用正确的锁（如 spinlock）及相应的内存同步规则。

因此：

1. **不要用 `Forbid()` 代替跨核锁**。
2. 不要假设关闭本核中断能阻止其他核心并行访问内存。
3. 共享结构的所有读/写方都必须遵循同一锁协议。
4. 在内核和驱动中需按目标架构使用原子操作、内存屏障以及缓存一致性机制。

## 6. AmigaOS 3.x 的多任务管理

经典 AmigaOS 3.x 的设计目标是**单 CPU 上响应迅速、内存开销小的抢占式多任务**：

- Exec 管理 Task 列表、任务状态和上下文切换。
- Task 根据优先级执行并通过 Wait/Signal 等机制协作。
- 多个应用并行“推进”工作，但标准系统不会把不同 Task 同时安排到不同 CPU 核心。
- 全局共享的地址空间降低通信成本，也让同步和内存错误变得危险。

## 7. AmigaOS 4.x 的多线程与 SMP 现状

### 7.1 ExecSG

AmigaOS 4.x 的 PowerPC 内核称为 **ExecSG（Exec Second Generation）**，继承 Exec 的多任务 API 并加强了内存系统、信号量、互斥量等基础机制。

**不能因为一台 PowerPC 机器具有双核/多核硬件，就认为官方 ExecSG 一定能够在所有核心上同时调度普通应用 Task。**

### 7.2 官方系统与项目演示必须区分

- 2024 年公开开发交流中，ExecSG 的多核支持仍被描述为尚未稳定工作。
- AmigaOS 4.1 Final Edition Update 3 后续正式发布的主要更新涉及稳定性、驱动与缓存/DMA 等；不能把这些内容解释成正式完成 SMP。
- 2026-09-28 的公开报道指出：第三方开发者在 AmigaOS 4 上展示了拟替换 ExecSG 的开源内核，多核同时运行已有演示；报道当时仍对具体机制、覆盖范围和成熟度持保留态度。

**截至 2026-11-06 的保守结论**：正式、广泛使用的 AmigaOS 4.1/ExecSG 不能被描述成已普遍具备成熟 SMP；第三方替代内核应单独跟踪，不与官方发行版混写。

## 8. AROS 的 SMP 现状

### 8.1 常规构建和 SMP 构建不同

AROS 官方平台资料列出：

- 常规 `pc-i386`、`pc-x86-64` 等移植。
- **`pc-x86_64-smp` 实验性原生移植**：官方状态为 *Working, incomplete driver support*（可运行，但驱动支持不完整）。

因此正确说法是“**AROS 有 SMP 实现，但不能假定每个 AROS 镜像都开启了 SMP**”。

### 8.2 x86-64 SMP

AROS 2020 年的官方开发消息提到，其实验性 x86-64 SMP 调度器经过改写，使用共享的待运行任务队列，并允许指定任务可运行的核心。

这只是对该阶段实现的描述；不应将其当作所有 AROS 架构和未来版本都固定不变的调度算法。

### 8.3 ARM/AArch64 SMP

AROS 官网近年开发动态进一步描述了 ARM/AArch64 的 SMP 工作，包括：

- 启动辅助 CPU 核心（secondary core bring-up）。
- 核间中断（IPI），包括树莓派平台相关中断控制器的支持。
- 自旋锁、内存屏障及支持 SMP 的分配器。
- 每 CPU 调度器（per-core scheduler）。
- CPU 亲和性支持，例如 `NP_Affinity`、`SetTaskAffinity()`。
- 对调度、定时器、信号量、设备 I/O、网络和内存映射的 SMP 测试。

这些能力表明 AROS 的 SMP 开发已经超出纯概念阶段，但**具体硬件、驱动、构建和稳定性仍需分别确认**。

### 8.4 Hosted AROS 不等于原生 AROS SMP

运行在 Linux/Windows/macOS 上的 hosted AROS 可以依赖宿主系统的线程和调度机制。但不能据此推断 AROS 自己的原生 Exec 调度器在所有 hosted/native 构建中都具备同样的多核能力。

## 9. 真正实现 SMP 需要解决什么？

下表概括从经典单核 Exec 走向 SMP 所需的工程工作：

| 子系统 | SMP 的额外要求 |
| --- | --- |
| CPU 初始化 | 发现并启动辅助 CPU 核心 |
| 调度器 | 每核可运行队列或共享队列、任务状态原子更新、迁移机制 |
| 中断 | 每核中断状态、核间中断（IPI） |
| 锁与同步 | Spinlock、Semaphore、Mutex、原子操作、内存屏障 |
| 内存分配器 | 在多核同时申请/释放内存时保持一致性 |
| 缓存及 MMU | 缓存一致性、页表修改同步、必要的 TLB 失效处理 |
| 驱动程序 | 保证设备队列、DMA、中断回调在多核环境中安全 |
| 兼容性 | 处理旧代码对全局数据、`Forbid()` 和隐式单核性的依赖 |
| 调试与测试 | 竞争条件、死锁、跨核时序错误、压力测试 |

为什么 SMP 对 Amiga 风格系统尤为困难？**因为历史代码广泛共享指针和全局结构，并存在依赖“当前 CPU 禁止切换即可暂时排他”的写法。** 在真正多核上，其他 CPU 不会因为某个核心暂停调度而自动停止。

## 10. 如果要编写多任务程序

推荐的基本架构：

```text
┌──────────────────┐       Message Port        ┌──────────────────┐
│  UI / Main Task  │ ────────────────────────► │   Worker Task    │
│                  │ ◄──────────────────────── │                  │
└──────────────────┘      Reply / Result       └──────────────────┘
          │                                          │
          └──────────共享缓冲区（受锁保护）──────────┘
```

设计要点：

1. 先清楚划分 Task、消息端口及共享对象的**所有权**。
2. 不用忙等待循环替代 `Wait()` 或其他事件驱动机制。
3. 给每个 Task 分配足够的栈空间，并负责其生命周期。
4. 不要无意义地提高优先级。
5. 用消息协议或正确的互斥原语同步；勿依赖 `Forbid()` 作为跨核锁。
6. 如果目标包含 AROS SMP，使用目标 SDK 提供的多核安全 API，不要假设传统链表操作天然原子。
7. 明确平台是否实际开启多核，再决定是否使用 CPU affinity 等特性。
8. 对共享内存、消息指针、数据缓冲区和驱动回调进行竞争条件测试。

## 11. 常见误解

| 误解 | 正确理解 |
| --- | --- |
| “AmigaOS 可以同时打开多个软件，所以支持多核。” | 多任务调度与多核同时执行是两回事。 |
| “有两个 CPU 核心，OS 就会用到两个。” | 必须由内核主动初始化和调度核心。 |
| “AROS 一直没有 SMP。” | 已有实验性 x86-64 SMP，ARM/AArch64 也有相关开发成果。 |
| “AROS 的所有发行版都支持 SMP。” | 不正确，必须看具体构建与驱动。 |
| “Forbid 就是跨核互斥锁。” | 不正确，AROS EXECSMP 中只限制本核心的任务切换。 |
| “Message Port 会自动复制消息。” | 通常是传递消息对象的引用/指针，须遵守生命周期规则。 |
| “AmigaOS 4 的 Slab 支持多核扩展，所以 ExecSG 已完成 SMP。” | 分配器设计并不能证明整套系统具备可用 SMP。 |

## 12. 参考资料

### 官方/开发者资料

1. [AmigaOS Wiki — Introduction to Exec](https://wiki.amigaos.net/wiki/Introduction_to_Exec)
2. [AmigaOS Wiki — Exec Tasks](https://wiki.amigaos.net/wiki/Exec_Tasks)
3. [AmigaOS Wiki — Exec Signals](https://wiki.amigaos.net/wiki/Exec_Signals)
4. [AmigaOS Wiki — Exec Semaphores](https://wiki.amigaos.net/wiki/Exec_Semaphores)
5. [AmigaOS Wiki — Exec Mutexes](https://wiki.amigaos.net/wiki/Exec_Mutexes)
6. [AROS — Supported Ports](https://www.aros.org/introduction/ports.html)
7. [AROS — Exec API Autodocs（含 EXECSMP 注意事项）](https://developers.aros.org/documentation/autodocs/exec.html)
8. [AROS — Project Homepage / Recent Development](https://www.aros.org/)
9. [AROS — 2020 news archive（早期 x86-64 SMP 进展）](https://www.aros.org/el/news/archive/2020.html)
10. [Hyperion Entertainment — AmigaOS 4.1 FE Update 3 发布信息](https://www.hyperion-entertainment.com/index.php/)

### 社区报道（不应等同于官方正式功能声明）

11. [amiga-news.de — 2024 年 ExecSG 多核开发状态](https://www.amiga-news.de/en/news/AN-2024-10-00114-EN.html)
12. [amiga-news.de — 2026-09-28 AmigaOS 4 第三方替代内核演示](https://www.amiga-news.de/en/news/AN-2026-09-00113-EN.html)

---

**结论**：经典 AmigaOS/Exec 的主要能力是低开销抢占式多任务，AmigaOS 4.x 的正式 ExecSG 仍不应被等同于成熟 SMP 内核；AROS 则具备实验性且持续发展的多核调度实现。要评估“是否真正多核”，必须同时核对**构建类型、内核调度、共享内存同步和驱动成熟度**。
