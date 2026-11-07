# 17｜安全性、稳定性、隔离与旧软件兼容取舍

## 现有系统机制

经典 AmigaOS 以低开销共享内存和 Task 间直接协作为核心，通常没有现代 Linux/Windows 那种默认强制的进程地址空间隔离。AmigaOS 4.x 使用 MMU 和改进后的内存系统，但不能视为普遍完整进程隔离。AROS 的内存保护与 SMP 特性依平台、构建和实现阶段而异。

**结果**：许多经典 Amiga 程序默认可以传递共享指针，并可能依赖全局结构或 `Forbid()` 一类单核行为。如果你的目标 OS 同时追求安全隔离和旧程序二进制兼容，需要明确冲突解决策略。

## 三类兼容/安全策略

| 方案 | 优点 | 缺点 |
| --- | --- | --- |
| Classic shared address space | 易兼容经典消息指针，低开销 | 故障隔离和安全边界弱 |
| Protected native processes | 强隔离、易恢复 | 需要 IPC 数据序列化或共享映射，旧 API 不可直接照搬 |
| Dual-mode / emulator sandbox | 可以兼顾新 native API 和旧 ABI | 工程复杂，需要 ABI/设备服务代理 |

**建议**：首版为了跑通可采用同地址空间，但从一开始就用不透明 Handle、引用计数和消息所有权；后续在保护模式下由服务代理处理需要共享指针的接口。

## 关键威胁与防护

### 内存与执行

- 所有外部长度、offset 进行溢出/边界检查。
- 用户态执行（若有）采用 W^X、不可执行栈、映射权限。
- 加载器严格检查可执行格式，资源清理遵守逆序回滚。
- 关键服务使用可观测 panic、core dump 或任务崩溃报告。

### 文件系统

- 路径规范化后再执行权限与卷范围检查，避免 `..`/Assign 跳出预期路径。
- 文件删除与枚举采用稳定生命周期，避免 use-after-free。
- 持久写入需要同步策略及断电恢复测试。
- 不允许未授权普通程序直接写物理块设备。

### 驱动与 I/O

- DMA 由统一映射接口管理，不把应用指针直接作为 DMA 地址。
- IRQ handler 不能直接访问已解除映射的 MMIO。
- 设备卸载和进程退出必须取消并清空在途 I/O。
- 禁止用户输入直接作为硬件寄存器偏移或内核函数指针。

### GUI 与输入

- 普通应用不应该读取所有窗口输入事件。
- 全局快捷键/输入过滤应有明确授予和撤销机制。
- 图标 ToolTypes、DataTypes、网络内容都属于不可信输入。
- 可选剪贴板和拖放能力要定义安全规则。

### 网络

- 不可信报文需严格解析并限制长度、层数和分配额。
- 不自行实现密码原语；使用维护良好的 TLS/加密库。
- 默认不开放不必要的监听端口，提供连接超时与速率限制。

## 故障恢复框架（建议）

```text
Failed app → terminate task/process
           → cancel outstanding IORequests
           → drop ports & locks
           → release memory and open libraries
           → report to system error service

Failed critical kernel component → collect logs → safe panic/reboot
```

旧式 Exec Task 的资源不一定由内核自动追踪并完全释放，**如果你要实现自动恢复，需要主动建立 owner/resource registry**。

## 测试原则

| 目标 | 通过条件 |
| --- | --- |
| 空指针、悬空引用防护 | 不造成不可定位的系统崩溃 |
| 异步请求取消 | 最终完成/取消且无双重释放 |
| 无权限设备访问 | 被拒绝并记录 |
| 格式解析器输入 fuzz | 不越界、不挂起 |
| 高优先级任务异常 | 有可诊断的饥饿/看门狗策略 |
| 网络断开/设备移除 | 所有等待者得到终态 |
| SMP 锁竞争 | 无死锁、无非法链表状态 |

## 实施建议

1. 将**安全边界**写在架构与 ABI 设计文档里，别等应用出现后才补。
2. 让“不可信数据的解析”尽可能在可替换、可 fuzz 的模块运行。
3. 故障恢复需要资源归属、引用计数和生命周期，不是简单 `KillTask`。
4. 对 `Forbid()`/`Disable()` 的兼容实现，应与 SMP 设计一起评估。
5. 核心内核服务保留必要最小权限；驱动和应用越少直接接触 MMIO 越好。

## 参考

- [AROS FAQ — compatibility and protection tradeoffs](https://www.aros.org/documentation/faq.html)
- [AROS Exec API](https://developers.aros.org/documentation/autodocs/exec.html)
- [AmigaOS Exec Device I/O](https://wiki.amigaos.net/wiki/Exec_Device_I/O)
