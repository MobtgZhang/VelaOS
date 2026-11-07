# 01｜系统总体架构与模块契约

## 现有系统机制

经典 AmigaOS 由 **Exec**（任务、内存、库、设备）、**AmigaDOS**（文件、Process、Shell）、**graphics/layers/intuition**（渲染和窗口）、**Workbench**（桌面）等构成。AROS 是对 AmigaOS 3.1 API 的开放重实现，通过平台抽象和 HIDD 适配不同硬件。

不要照搬 Linux 的“每一项 OS 服务都是独立受保护用户进程”假设；传统 Exec 任务及库运行于高度共享的地址空间。相反，你可以在新系统中主动设计保护层。

## 建议总体分层

```text
User applications / desktop / shell
     ├─ API ABI: Exec, DOS, Intuition, Graphics, Utility
     ├─ Services: filesystem handlers / audio mixer / network stack
     ├─ Kernel core: task, scheduler, signals, ports, memory, IRQ, timers
     ├─ Device layer: Exec I/O requests, HIDD-like objects, bus manager
     └─ HAL: CPU traps, MMU, clock source, IRQ controller, framebuffer
```

### 独立模块接口合同

| 模块 | 输入 | 输出 | 需要先完成 |
| --- | --- | --- | --- |
| HAL | 引导数据、硬件寄存器 | 内存图、CPU 初始化、异常、时钟 | boot |
| Exec | 可运行任务、内存、信号 | 调度、消息端口、库注册 | HAL |
| Device | IORequest、设备对象 | 完成回复/错误码 | Exec |
| DOS | 路径、文件句柄、Packet | 读写、目录、Assign | Exec + Device |
| Graphics | bitmap、绘图命令 | 像素区域与刷新 | framebuffer |
| Layers | layer、损坏区域 | 裁剪与重绘区域 | Graphics |
| Intuition | 输入事件、窗口对象 | IDCMP 事件、窗口绘制 | Layers + Input |
| Desktop | DOS 文件枚举、图标对象 | 文件浏览、程序启动 | DOS + Intuition |

## 代码组织建议（不是 AROS 固定源码目录）

```text
src/
├── boot/        # loader glue / early init
├── arch/x86_64/  # syscall, traps, paging, irq
├── kernel/       # exec, scheduler, memory, IPC
├── drivers/      # bus, block, console, input, audio, network
├── dos/          # handles, packets, assigns, shell
├── graphics/     # bitmap, raster, layers
├── intuition/    # screens, windows, IDCMP, gadgets
├── desktop/      # file manager, icons
├── libs/         # utility, locale, datatypes ...
├── include/      # 稳定的外部 API
└── tests/
```

## 必须及早固定的约束

- **平台**：首版 x86-64 UEFI/Multiboot2，单 CPU，QEMU；不同时设计三种 CPU。
- **指针**：`uintptr_t` 等可容纳指针的整数类型；不要用 32 位 `ULONG` 存 64 位指针。
- **字节序**：磁盘/网络数据使用固定字节序，应用内部采用 CPU 本机序。
- **公共 ABI**：头文件版本、结构体对齐、函数向量顺序、枚举常量不可随意改。
- **资源所有权**：`owner`、引用计数、完成/取消状态要明确。
- **错误处理**：设备 I/O 与 DOS API 分别保留原生语义，再向应用提供统一诊断层。

## 最小可运行验收

- QEMU 可以启动并输出日志。
- 2 个 Task 能被抢占式调度，并通过消息端口交换数据。
- `SYS:` 能映射到只读 RAMFS，Shell 能 `DIR`、`TYPE`。
- 图形缓冲区能绘制两层重叠矩形。
- 输入可激活不同窗口、移动窗口并触发关闭事件。
- 一个可加载的库能够被两个应用 `OpenLibrary` 并正确释放。

## 参考

- [AmigaOS Intuition Library](https://wiki.amigaos.net/wiki/Intuition_Library)
- [AROS Developer Introduction](https://developers.aros.org/documentation/app-dev/introduction.html)
- [AROS Portability FAQ](https://www.aros.org/documentation/faq.html)
