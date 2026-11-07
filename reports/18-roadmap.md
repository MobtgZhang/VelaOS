# 18｜从零实现一个 Amiga 风格 OS：依赖图、阶段任务与验收

> 本文给出**可落地的实现顺序**，不提供不可靠的工期估计。建议先确定 `00-README.md` 中的目标模式和 CPU 平台。

## 阶段 0｜冻结项目约束

**决策：**

- [ ] 选择 `x86-64 + QEMU` 作为首版目标。
- [ ] 采用自己的 native ABI；经典兼容放入后续里程碑。
- [ ] 第一版单核，后续以 `SMP.md` 中的准则增加多核。
- [ ] 选择 bootloader 协议、内核 ELF 链接模型、初始文件系统镜像格式。
- [ ] 定义公共整数类型、错误码、日志规范、任务/请求所有权。

**验收**：代码仓库有 `ARCHITECTURE.md` 或对应设计说明，所有模块依赖清晰。

## 阶段 1｜能启动的内核

- [ ] 串口日志与 panic。
- [ ] boot info、页表、物理页分配、kernel heap。
- [ ] 异常向量与 IRQ 初始化。
- [ ] 单调时钟、定时器。
- [ ] 抢占式 Task 切换。

**验收**：QEMU 输出 `boot ok`；两个 Task 交替打印；页故障可诊断。

## 阶段 2｜Exec 兼容核心

- [ ] Node/List、Task、Signal、MsgPort、ReplyMsg。
- [ ] `OpenLibrary()` 注册表和版本管理。
- [ ] Exec Device 基类，I/O 状态机。
- [ ] 同步与异常路径，避免丢唤醒。
- [ ] `AllocMem/AllocVec` 兼容或适配层（参见 `memory.md`）。

**验收**：两个应用能并发打开 `hello.library`，交换 10 万条消息；取消请求不会双重回复。

## 阶段 3｜文件系统和 Shell

- [ ] initrd + RAMFS。
- [ ] `dos.library` 基础文件 API。
- [ ] DOS Process 的当前目录和标准 I/O。
- [ ] 卷与 Assign 解析。
- [ ] Handler + Packet。
- [ ] ELF loader（native）、Shell 与 Startup-Sequence。

**验收**：Shell 的 `DIR SYS:`、`TYPE`、`COPY`、`ASSIGN` 可以使用；应用可从磁盘映像加载。

## 阶段 4｜软件图形桌面

- [ ] Framebuffer、BitMap、RastPort。
- [ ] Layers 裁剪与 damage region。
- [ ] input.device 的键鼠事件分发。
- [ ] Intuition Screen/Window、IDCMP。
- [ ] Button/TextBox/Menu/Requester 的最小集。
- [ ] 文件管理器，图标、双击、DefaultTool。

**验收**：图形桌面打开两个可交互窗口，可拖动、调整、关闭，双击启动程序。

## 阶段 5｜实用硬件能力

- [ ] PCI/VirtIO 设备管理。
- [ ] 可写块设备与 FAT。
- [ ] 声卡 PCM + AHI 适配。
- [ ] Ethernet + IPv4/UDP/TCP + bsdsocket 适配。
- [ ] Preferences / Locale / DataTypes。
- [ ] USB/HID 的可维护驱动。

**验收**：虚拟硬盘持久化、播放音频、网络回环通信、载入图片，重启后配置仍在。

## 阶段 6｜兼容与现代化

- [ ] 视目标决定 m68k/HUNK 或其他传统 ABI 支持。
- [ ] 审核全部公开结构体、TagItems、BPTR、函数向量。
- [ ] AROS API 兼容测试。
- [ ] 安全/隔离方案（可选独立地址空间）。
- [ ] SMP：只有在锁、驱动、IPI、定时器和内存分配安全后才启用。
- [ ] 热拔插、崩溃恢复、动态模块卸载等复杂功能。

**验收**：兼容 API 测试套件通过；多核压力测试中无可重现死锁和内存破坏。

## 系统依赖图

```text
Boot + HAL
     │
Memory ─ IRQ/Timer ─ Scheduler
     │             │
     └───── Exec IPC ───── Library Registry
                 │
              Device I/O
                 │
          DOS Handler + VFS ── Native Loader
                 │                   │
               Shell  ───────────────┘
                 │
           Graphics + Layers
                 │
Input ───────── Intuition ───────── Desktop
                 │
     Locale / Icons / DataTypes / Zune

Driver framework ── Storage / Audio / Network
```

## 交付标准（Definition of Done）

每个模块至少完成以下 6 项，才算“已实现”：

1. 公开 API 文档、输入输出与错误状态。
2. 生命周期（创建/打开/关闭/取消/销毁）写入状态机。
3. 单元测试覆盖正常与失败路径。
4. QEMU 集成测试可重复通过。
5. 在日志中可追踪该模块的故障和资源占用。
6. 版本、兼容性与已知限制有明确说明。

## 最小版本功能总表

| 版本目标 | 可用能力 | 明确暂不支持 |
| --- | --- | --- |
| MVP-1 | 启动 + Task + MsgPort + RAMFS + Shell | 实盘、桌面、SMP |
| MVP-2 | Libraries + Handler + Framebuffer + 键鼠 | GPU、复杂输入法 |
| MVP-3 | 窗口管理 + Desktop + Icons + Prefs | 网络、音频、多核 |
| Beta | 块设备、网络、音频、USB 基本驱动 | 全 ABI 兼容 |
| Compatibility Release | 目标 API/ABI 适配、兼容测试 | 无法保证跨 CPU 直接运行旧指令 |
| Multi-core Release | per-core 调度、IPI、跨核锁和驱动 | 不把旧 `Forbid` 当成全局锁 |

## 与已有三份文档的关系

- `system.md` → `SYS:`、`LIBS:`、`DEVS:`、`S:` 目录规划。
- `memory.md` → 分配器、地址空间、MMU、内存池。
- `SMP.md` → Task、调度、锁、IPI 与多核演进。
- 本系列 → 把上述基础能力连接到可运行的完整 OS 子系统。

## 参考

- [AROS Developer Documentation](https://developers.aros.org/)
- [AROS Modules](https://developers.aros.org/documentation/sys-dev/modules.html)
- [AROS Compiling](https://developers.aros.org/documentation/compiling.html)
- [AmigaOS Documentation Wiki](https://wiki.amigaos.net/wiki/Main_Page)
