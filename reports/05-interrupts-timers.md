# 05｜中断、时钟源、定时器与延迟工作

## 现有系统机制

经典 Amiga 有硬件中断管理、定时器和 `timer.device`；后者支持延时请求、系统时间查询、时钟读数等。不同 AmigaOS 版本具有不同的 timer 结构定义与时间 API；AmigaOS 4.x 文档提醒旧 `timeval` 与 POSIX 类型冲突。

AROS 原生移植以硬件相关层适配 PIC/APIC/GIC 等不同中断控制器；多核版本还涉及核间中断（IPI）。

## 中断处理分层（建议实现）

```text
IRQ controller → CPU exception/trap entry
                        │
                    ACK / Mask
                        │
                  hard IRQ handler
                        │
             enqueue deferred work
                        │
                  return from IRQ
                        │
                 worker / device task
                        │
               ReplyMsg / Signal
```

### IRQ 上下文禁忌

- 不睡眠、不执行阻塞文件操作。
- 不访问未做好中断并发保护的普通对象。
- 不在 hard IRQ 中长期混音、绘图或执行网络协议栈。
- 若使用来自普通堆的内存分配，须确认该分配器明确可在 IRQ 上下文调用。
- 清除/确认设备中断顺序应按硬件手册执行，否则可能产生中断风暴。

## Timer 服务接口（建议）

```c
uint64_t monotonic_ns(void);
uint64_t realtime_ns(void);
TimerId timer_schedule_abs(uint64_t deadline_ns, TimerCallback cb, void *arg);
bool timer_cancel(TimerId id);  // 取消成功不等于回调未并发执行
void arch_timer_program(uint64_t deadline_ns);
```

这些是新系统的内部接口示意，不是 Exec 官方函数。

### 时间语义

- **Monotonic**：单调递增，适合超时、调度与性能计数。
- **Realtime**：可被用户/NTP 修正，适合日历与文件时间戳。
- **Tick**：可以是调度时基，不一定等于墙上时钟。
- **Deadline**：推荐在内核中使用绝对单调时间，避免相对超时重复累计误差。

## timer.device 兼容表层

| Amiga 风格功能 | 你的内核实现 |
| --- | --- |
| `TR_ADDREQUEST` | 把 TimeRequest 加到 deadline 队列 |
| `TR_GETSYSTIME` | 查询墙上时间 |
| `TR_SETSYSTIME` | 需要授权的时间设置 |
| `UNIT_VBLANK` | 经典屏幕刷新同步语义（按平台实现） |
| `UNIT_MICROHZ` / `UNIT_ECLOCK` | 高精度延时/硬件时间源抽象 |

**注意：** 不同硬件的 VBLANK 与 E-Clock 特性不同；不能随意把现代 APIC 频率等同于经典 Amiga E-Clock。

## 数据结构

```text
TimerQueue:
    min-heap by absolute_deadline
    cancellation generation / state
    callback context
    optional per-CPU queues for future SMP
```

## 先后顺序

1. 建立 CPU exception 与 IRQ 入口。
2. 选择可靠的单调时钟源。
3. 提供一个硬件定时唤醒源。
4. 实现超时堆、唤醒 Signal、worker 回调。
5. 接入 `timer.device` 兼容接口。
6. 后续 SMP 再补核间同步、per-CPU timer、时钟跨核一致性。

## 验收

- 1000 个同时到期/取消的请求均有且仅有一次终态。
- 关机时取消全部待处理回调。
- 负载下定时器可以延迟，但不得无故提前触发绝对 deadline。
- IRQ storm 能记录来源并自动防护/限制。
- SMP 中运行队列和定时器队列无竞争条件。

## 参考

- [AmigaOS Timer Device](https://wiki.amigaos.net/wiki/Timer_Device)
- [AmigaOS Input Device](https://wiki.amigaos.net/wiki/Input_Device)
- [AROS Scheduling](https://developers.aros.org/documentation/sys-dev/scheduling.html)
