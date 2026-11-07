# 06｜设备模型、Exec Device I/O、HIDD、总线与 DMA

## 现有系统机制

**Exec Device 与 AmigaDOS Handler 是不同层次。**

- **Exec Device**（如 `trackdisk.device`、`console.device`）：通过 `OpenDevice()` 和 `IORequest` 提供同步/异步命令接口。
- **DOS Handler**（如 `CON:`、`RAM:`、磁盘文件系统）：以 DOS 名称空间提供 `Open/Read/Write` 等高层文件接口，可建立在 Exec Device 上。
- **AROS HIDD**：Hardware Independent Device Driver，提供面向硬件类别的抽象；AROS 文档还描述了 PCI HIDD / 驱动模块体系。

## Exec I/O 的状态机

```text
NEW → OPENED → SUBMITTED → [QUICK_DONE] → REUSABLE
                          └→ QUEUED → IN_FLIGHT
                                        ├→ DONE → REPLIED
                                        └→ CANCEL_REQUESTED
                                                  └→ REPLIED
```

`DoIO()` 通常等待完成；`SendIO()` 提交异步请求；`WaitIO()` 获取完成结果；`AbortIO()` 尝试取消，**不能省略完成等待**。`IOF_QUICK` 可能表示调用方内联完成，无需发送普通回复消息；需要严格按原有协议实现。

### 典型调用时序

```text
Client:
  create MsgPort / IORequest
  OpenDevice("foo.device", unit, req, flags)
  set io_Command / io_Data / io_Length
  SendIO(req)
  perform other work
  WaitIO(req)     # 或 CheckIO / WaitPort
  CloseDevice(req)
  free IORequest / MsgPort
```

## 推荐框架分层

```text
Applications
    │  Exec Device API
    ▼
Device Manager ──────────────── device registry
    │
Generic Driver/HIDD API ─────── bus/device class
    │
PCI/USB/VirtIO bus manager
    │
Controller driver → MMIO/PIO + DMA + IRQ
    │
Physical / virtual device
```

**内部接口示意：**

```c
struct DriverOps {
    int (*probe)(DeviceInfo *);
    int (*open)(Device *, unsigned unit);
    int (*submit)(Device *, IORequest *);
    int (*cancel)(Device *, IORequest *);
    void (*remove)(Device *);
};
```

## PCI/USB 与设备生命周期

1. 枚举设备 ID、资源（BAR/MMIO/IRQ）及设备类别。
2. 为对应驱动完成 `probe`，分配资源与 DMA 队列。
3. 注册设备对象，但先不要暴露未初始化完成的对象。
4. 在热拔出/驱动卸载时先停止新请求，再取消/完成旧请求。
5. 所有在途请求、映射和 IRQ handler 清理后才释放设备。

AROS 的 PCI HIDD 手册讨论属性查询、BAR 及 `PCIDriver` 类；这些属于 AROS 已有接口，**不必与你自设计的 `DriverOps` 一模一样**。

## DMA/缓存安全

- 不要假定 CPU 虚拟地址可直接作为总线 DMA 地址。
- DMA 缓冲区应有“映射→同步→设备使用→同步→解除映射”生命周期。
- 需要区分物理连续性、对齐、地址位宽、IOMMU、缓存一致性。
- USB/HID/音频/网络驱动不能在设备读写缓冲区时无保护地释放内存。

## 最小驱动优先级

1. 串口/调试控制台
2. 帧缓冲（优先使用 bootloader 已初始化的模式）
3. 虚拟键鼠（QEMU 设备）
4. `virtio-blk` 或简单 RAM block device
5. `virtio-net` 与虚拟音频
6. PCI / USB 扩展、真实硬件驱动

## 验收

- 并发 `SendIO` N 个请求，全部只完成一次。
- `AbortIO` 与完成竞争不触发双重回复或悬空引用。
- 驱动初始化失败能完整回滚 IRQ、MMIO、DMA 和注册项。
- 拔出设备时客户端得到明确错误，不会永久等待。
- 不同 unit 之间保持状态隔离。

## 参考

- [AmigaOS Exec Device I/O](https://wiki.amigaos.net/wiki/Exec_Device_I/O)
- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS PCI HIDD Manual](https://developers.aros.org/documentation/hardware/pci.html)
- [AROS Modules](https://developers.aros.org/documentation/sys-dev/modules.html)
