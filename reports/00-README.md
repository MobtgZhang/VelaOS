# AmigaOS / AROS 风格操作系统：实现指南

> 版本：2026-11-06｜用途：为**自行实现**一个 Amiga 风格操作系统提供模块化设计、接口边界、工程顺序和验收清单。  
> 本套文档是技术设计参考，**不是已编译完成的操作系统源码**。

## 阅读前先确定目标

你需要在三种目标之间选择一种：

| 模式 | 目标 | 代价 |
| --- | --- | --- |
| **A. 兼容 AmigaOS 3.1** | 尽量保持经典 API/ABI、DOS 语义、Exec Task 与图形接口 | 受共享地址空间、BCPL/历史数据布局等兼容约束 |
| **B. AROS 风格现代移植** | 兼容 Amiga API，运行在 x86-64/ARM64，优先原生重编译 | 需要 HAL、HIDD、驱动和新 ABI |
| **C. Amiga 风格的新 OS** | 保留 Exec、DOS、Intuition 的设计思想，但自定安全与 ABI | 无法天然运行旧二进制，接口和语义要自己维护 |

**建议：** 首个可运行版本选 **C 模式、x86-64、QEMU、单核、64 位、软件 framebuffer、RAM 文件系统**；等系统稳定后再逐步兼容 Amiga API。若目标是运行真实 m68k Amiga 程序，应将 **m68k ABI、HUNK、端序和系统结构布局**作为独立兼容工程。

## 文档索引

| 文件 | 内容 | 建议阶段 |
| --- | --- | --- |
| `01-architecture.md` | 总体架构、核心依赖和边界 | 0 |
| `02-boot-hal.md` | 引导、HAL、CPU/异常初始化 | 1 |
| `03-exec-libraries.md` | Exec 对象、Library、Device、Resource、Resident | 2 |
| `04-ipc-signals.md` | 消息端口、Signal、锁及任务同步 | 2 |
| `05-interrupts-timers.md` | IRQ、中断下半部、timer.device、时钟 | 2 |
| `06-device-drivers.md` | Exec Device I/O、HIDD、总线与 DMA | 3 |
| `07-dos-vfs.md` | dos.library、DOS Process、Assign、Handler、Packet | 3 |
| `08-storage-filesystems.md` | 磁盘、分区、RAMFS、FAT、文件系统 | 3 |
| `09-graphics.md` | graphics.library、RastPort、BitMap、Layers | 4 |
| `10-intuition-desktop.md` | Screens/Windows、IDCMP、BOOPSI、桌面 | 4 |
| `11-input.md` | keyboard/mouse/input.device、焦点、Commodities | 4 |
| `12-audio.md` | audio.device、AHI、混音、声卡驱动 | 5 |
| `13-network.md` | NIC、TCP/IP、bsdsocket.library | 5 |
| `14-system-services.md` | Locale、DataTypes、Icon、Shell、Preferences | 5 |
| `15-abi-loader.md` | ELF/HUNK、ABI、端序、模块装载与兼容 | 3–6 |
| `16-build-debug-test.md` | 工具链、调试、CI 和仿真测试 | 贯穿 |
| `17-security-reliability.md` | 故障隔离、安全边界、资源回收、稳定性 | 贯穿 |
| `18-roadmap.md` | 实现顺序、模块清单、阶段验收 | 总计划 |
| `19-api-checklist.md` | 常见兼容 API 逐项验收 | 贯穿 |
| `20-implementation-status.md` | AmigaOS 3.x/4.x/AROS 模块实现状态矩阵 | 调研 |

与此前内容配套：`system.md`（目录）、`memory.md`（内存）、`SMP.md`（多任务及多核）。**前三份不在本次压缩包内，属于你先前获得的文档。**

## 子系统全景

```text
+------------------ Applications / Shell / Desktop ------------------+
| Workbench / Wanderer-like | CLI | GUI Apps | Datatypes | Network |
+------------------------- API & Libraries -------------------------+
| dos.library | intuition.library | graphics.library | utility     |
| layers | icon | locale | datatypes | audio/AHI | bsdsocket       |
+---------------------------- Services -----------------------------+
| Exec Task/Port/Signal | DOS Handlers | Timer | Input | Resource   |
+------------------------- Driver Framework ------------------------+
| Exec Devices | HIDD | PCI / USB / VirtIO | Block | NIC | Sound    |
+--------------------------- Kernel / HAL --------------------------+
| CPU | IRQ | Memory/MMU | Scheduler | IPC | Clock | Boot         |
+------------------------------- HW ---------------------------------+
```

在历史系统中，这些部件不严格对应现代的“内核态/用户态”二分；图仅表达**依赖层次**，不是安全边界。

## 功能状态说明

- **AmigaOS 3.x**：Exec、AmigaDOS、Intuition、Graphics、Layers、Workbench 和设备框架均为经典成熟组件；通常缺乏现代进程隔离，标准系统非 SMP。
- **AmigaOS 4.x**：ExecSG、MMU 管理、现代化硬件支持和接口机制增强；不应默认认为正式系统具有完整进程隔离或普遍可用的 SMP。
- **AROS**：有开源重实现、HIDD、Wanderer、Zune、现代硬件移植；支持情况随架构/驱动/构建变化，有实验性 SMP。2026 年还在持续演进 PCI、USB、文件系统等组件。

## 开发原则

1. **接口先行**：先定义公开头文件、对象状态、错误代码、所有权、取消协议，再写实现。
2. **先单核后多核**：MVP 先跑通单核，所有共享结构从设计开始考虑后续锁和并发。
3. **先 RAMFS 后磁盘写入**：尽量避免早期驱动导致物理磁盘数据损坏。
4. **先软件绘制后 GPU**：先验证 framebuffer/剪裁/窗口事件，再添加加速驱动。
5. **先 native ABI 后兼容层**：旧二进制兼容涉及 CPU、端序、指针宽度、运行时及装载器，不可用简单重编译概括。
6. **区分描述与建议**：每个文件用“现有系统机制”和“建议实现”两种标题，不把建议方案冒充 AROS 原生实现。

## 主要官方参考

- [AROS Introduction](https://aros.org/introduction/)
- [AROS Developer Portal](https://developers.aros.org/)
- [AROS Platform/Ports](https://www.aros.org/introduction/ports.html)
- [AROS Modules](https://developers.aros.org/documentation/sys-dev/modules.html)
- [AmigaOS Documentation Wiki](https://wiki.amigaos.net/wiki/Main_Page)

> 源码和 SDK 细节会变化；在实现具体接口之前，请以**目标版本的头文件与源码**确认名称、结构布局和调用约定。
