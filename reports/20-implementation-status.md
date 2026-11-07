# 20｜AmigaOS 与 AROS 其他模块的实现状态对照（2026-11）

> **使用方法**：这张表用来回答“原系统到底实现到什么程度”，其他文档回答“你应该如何实现”。  
> **注意**：AROS 是多架构、多构建、多发行版项目；“有源代码实现”不等于“你使用的镜像里一定已启用、硬件一定可用或生产环境已稳定”。

## 1. 核心系统

| 模块 | AmigaOS 3.x | AmigaOS 4.x | AROS | 实现时最重要的边界 |
| --- | --- | --- | --- | --- |
| 启动与平台初始化 | Kickstart/经典固件，成熟 | PowerPC 平台固件与磁盘 Kickstart 模块 | 原生/宿主多种移植，平台相关差异大 | HAL、页表、固件、CPU 启动 |
| Exec Task / 调度 | 成熟，经典单核 | ExecSG 成熟多任务，正式 SMP 有限制 | 成熟多任务，部分实验性 SMP | Task 生命周期、调度、安全同步 |
| 内存分配 | 成熟但共享内存为主 | MMU 和现代化分配器 | 分配 API 成熟，MMU/保护随平台不同 | 详见 `memory.md` |
| Library / Resource | 成熟 | 成熟，接口机制扩展 | 已重实现对应框架 | LVO、Interface、版本号、Open/Close |
| Signal / MsgPort | 成熟 | 成熟 | 已实现，SMP 构建需特别注意锁 | 不丢唤醒、消息所有权 |
| Timer / 中断 | 成熟 | 成熟，底层硬件不同 | 已有定时器与架构相关中断实现 | IRQ 上下文与取消流程 |

## 2. I/O、文件与驱动

| 模块 | AmigaOS 3.x | AmigaOS 4.x | AROS | 实现时最重要的边界 |
| --- | --- | --- | --- | --- |
| Exec Device I/O | 成熟 | 成熟 | API/驱动框架已实现 | IORequest、DoIO/SendIO、Abort/Wait |
| DOS Handler/Packet | 成熟 | 成熟，接口进一步扩展 | 已有 DOS 和 Handler 体系 | DOS 名字空间≠Exec 设备 |
| 文件路径 / Assign | 成熟 | 成熟 | 已实现相近语义 | 路径解析、BPTR、锁与句柄 |
| 磁盘/分区 | 经典硬盘/软盘生态成熟 | 根据硬件支持 | 多控制器驱动；覆盖范围取决于构建 | PCI/USB、DMA、虚拟块设备 |
| FFS/FAT/SFS/PFS3 | 经典文件系统及扩展按安装而异 | 文件系统按系统/第三方组件而异 | 多种文件系统在维护，包括 2026 的 64 位/一致性修复 | 不要把某格式视为所有版本标配 |
| PCI / USB | 经典机型并非普遍自带现代总线 | 视机器及驱动 | HIDD/PCI 支持；2025–26 增强 USB 3.x/xHCI，具体驱动仍有差异 | 枚举、hotplug、DMA、安全卸载 |

## 3. 显示与用户界面

| 模块 | AmigaOS 3.x | AmigaOS 4.x | AROS | 实现时最重要的边界 |
| --- | --- | --- | --- | --- |
| graphics.library | 成熟的绘图/位图 API | 增强的图形机制 | 重实现并结合显示 HIDD | Raster、BitMap、颜色模式 |
| layers.library | 成熟 | 成熟 | 已实现 | 裁剪、damage、z-order |
| Intuition | 成熟窗口、Screen、IDCMP | 成熟并发展 | 已实现兼容窗口机制 | 输入焦点与事件生命周期 |
| BOOPSI / Gadgets | 成熟 | 扩展的控件体系 | 已实现相近对象体系 | 对象引用、方法和 TagList |
| Workbench / Wanderer | Workbench | Workbench | Wanderer | 桌面是应用层，不是内核 |
| Zune / MUI 样式 | MUI 通常为独立第三方环境 | 随安装组件而定 | Zune 是 AROS 重要 GUI 工具包 | 不应把 Zune 当成原生 AmigaOS 3.x 内核模块 |
| input.device / Commodities | 成熟 | 成熟 | 已有输入与过滤体系，物理设备覆盖不一 | 键码、Keymap、IDCMP、全局过滤 |

## 4. 多媒体与系统服务

| 模块 | AmigaOS 3.x | AmigaOS 4.x | AROS | 实现时最重要的边界 |
| --- | --- | --- | --- | --- |
| 音频 / AHI | audio.device 成熟；AHI 在经典环境常另行安装 | AHI 与现代音频设施 | 有音频子系统，声卡支持随移植而异 | 多流、混音、实时 IO |
| TCP/IP / socket | 第三方 TCP/IP 栈常见，并非所有 3.x 安装自带 | 系统集成的 TCP/IP 环境 | AROSTCP 开源协议栈已实现；2026 代码覆盖 IPv4/IPv6、TCP/UDP、路由等 | **软件栈成熟度和网卡驱动可用性分开看** |
| bsdsocket.library | 由相应网络栈提供 | 网络编程接口可用 | AROSTCP 的 BSD socket API 可用，并有 Roadshow 兼容扩展工作 | 文件描述符与 Task 信号的桥接 |
| DataTypes / IFF | 数据类型扩展框架成熟（依 OS 3.x 具体版本） | 成熟并扩展 | 已实现多种类型，格式覆盖仍有缺口 | 文件识别、解码安全 |
| Locale / Catalog | 已有本地化框架 | 成熟 | 已有本地化实现 | 编码、catalog 回退 |
| Icon / .info | 成熟 | 成熟，图标格式随版本演进 | 已有图标和桌面机制 | ToolTypes、DefaultTool、启动语义 |
| Shell / Prefs | 成熟 | 成熟 | 已实现对应工具/脚本环境 | Startup-Sequence、ENV/ENVARC |
| ARexx | 经典脚本自动化生态 | 具体组件随系统 | 兼容解释器/支持依安装而异 | 消息端口命令协议 |

## 5. 哪些是“优先复用现有设计”的部分？

**值得仿照公共 API：** Exec Library/Device、Signal/MsgPort、DOS Packet、Assign、Intuition IDCMP、BOOPSI、DataTypes。

**值得保留概念但使用现代实现：** HAL、MMU、虚拟内存、PCI/USB、DMA、图形合成、异步网络、文件系统日志、崩溃报告。

**需要谨慎处理经典假设：** 内存全局共享、`Forbid()` 当作锁、经典 BPTR、在结构体中存放裸指针、缺乏进程隔离的插件模型。

## 6. AROS 2026 网络的具体证据

AROS 项目维护的 AROSTCP 文档（更新至 2026 年）将 IPv4、IPv6、TCP/UDP、ICMP、ARP、SANA-II、BSD socket 等列为已实现，并记录 DHCPv6、IPv6 邻居发现、SLAAC 和防火墙等进展。它反映的是项目代码文档，**不是所有网卡、平台、发行包都经过完整认证**。

对于自己的 OS，最小版本仍建议先完成 IPv4 + UDP，再做 TCP，最后 IPv6；这属于**开发顺序建议**，并非在说 AROS 还没有 IPv6。

## 7. 参考来源

1. [AmigaOS Documentation Wiki](https://wiki.amigaos.net/wiki/Main_Page)
2. [AROS Introduction](https://aros.org/introduction/)
3. [AROS Developer Portal](https://developers.aros.org/)
4. [AROS Ports](https://www.aros.org/introduction/ports.html)
5. [AROS Module Types](https://developers.aros.org/documentation/sys-dev/modules.html)
6. [AROS Contributors — 未完成功能与驱动](https://developers.aros.org/documentation/contribute.html)
7. [AROS Project News (2025–2026)](https://www.aros.org/)
8. [AROSTCP — 项目源码配套技术文档（2026）](https://github.com/aros-development-team/AROS/wiki/AROSTCP)
9. [AmigaOS Developing Network Applications](https://wiki.amigaos.net/wiki/Developing_Network_Applications)
