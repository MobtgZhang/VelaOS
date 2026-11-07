# 02｜引导流程、HAL 与早期内核初始化

## 现有系统机制

- **AmigaOS 3.x**：经典机器的 Kickstart 固件提供 Exec 等基础模块，系统之后装载 AmigaDOS 与磁盘启动脚本。
- **AmigaOS 4.x**：依赖其平台固件和磁盘 `Kickstart` 模块，具体加载方式随机器而变。
- **AROS**：原生（native）移植需要自身启动和硬件初始化；宿主（hosted）构建借助宿主 OS，PC 镜像常由 GRUB 类引导器加载。AROS 构建/移植文档长期强调架构专用代码的独立性。

## 推荐首版启动路径

```text
UEFI/Multiboot2 bootloader
       ↓ 获得 framebuffer、内存图、命令行
BootInfo 校验（大小、版本、指针、映射）
       ↓
建立临时页表与内核堆栈
       ↓
CPU 异常 / IDT / 中断控制器 / 日志串口
       ↓
物理内存分配 → 虚拟映射 → Kernel heap
       ↓
时钟中断 → Task/IPC → 设备注册表
       ↓
RAMFS / DOS handler → Shell → GUI
```

**BootInfo 建议字段（伪代码，非 Amiga/AROS 原生 ABI）：**

```c
typedef struct {
    uint32_t magic, abi_version;
    uintptr_t memory_map; size_t memory_map_len;
    uintptr_t framebuffer; uint32_t fb_width, fb_height, fb_pitch;
    uintptr_t initrd; size_t initrd_size;
    uintptr_t acpi_rsdp;
} BootInfo;
```

## HAL 需要提供什么

| 接口（示意） | 用途 |
| --- | --- |
| `arch_early_init(info)` | 检查 CPU 模式、栈和基本映射 |
| `arch_install_traps()` | 设置异常向量、收集寄存器快照 |
| `arch_irq_register(vector, fn)` | 绑定中断处理例程 |
| `arch_timer_program(deadline)` | 启用下一次定时唤醒 |
| `arch_context_switch(old, next)` | 保存/恢复执行上下文 |
| `arch_map_pages(va, pa, flags)` | 建立 MMU 页表映射 |
| `arch_halt()` | 致命错误后停机 |
| `hal_framebuffer()` | 返回像素缓冲区及其格式 |

这些是**设计接口名称**，不是 AmigaOS 官方函数。

## 早期初始化必须特别注意

1. 中断启用前先确保 IDT、任务栈和日志系统可用。
2. 内存映射应拒绝覆盖内核代码、设备 MMIO、bootloader 保留区域。
3. 使用串口作为最小日志通道，便于窗口系统未启动时排障。
4. 初始化阶段允许静态内存池；内核 heap 尚未建立时不要调用一般分配器。
5. 原生驱动须使用设备提供的 MMIO/PIO 地址，不能假设所有设备可用恒等映射访问。
6. 引导错误要有明确 panic 原因与寄存器现场。

## 推荐分阶段实现

- **B0**：只有 `kmain()`，串口输出 `boot ok`。
- **B1**：页表、异常处理、静态分配器。
- **B2**：定时器、抢占式调度、空闲任务。
- **B3**：RAM initrd、shell、framebuffer。
- **B4**：ACPI/PCI、设备探测与真实磁盘。
- **B5**：多核启动在 `SMP.md` 的基础上另行开启。

## 验收

- 缺少 BootInfo/错误版本时清楚报错而非乱访问内存。
- 除零、非法指令和页错误能记录 CPU、异常向量和故障地址。
- QEMU 重复 100 次启动，未出现偶发死机。
- 不同内存大小（256MB、1GB、4GB）下能正确取得空闲物理页。

## 参考

- [AROS Ports](https://www.aros.org/introduction/ports.html)
- [AROS Compiling](https://developers.aros.org/documentation/compiling.html)
- [AROS System Porting（历史文档，旧命令勿直接照抄）](https://developers.aros.org/documentation/sys-dev/system-development.html)
