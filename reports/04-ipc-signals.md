# 04｜Signal、Message Port、Packet 与同步协议

## 现有系统机制

Exec 用 Signal 通知 Task 某种事件发生，用 Message Port 传送包含消息头的对象指针。设备异步 I/O 通过回复端口归还 IORequest；AmigaDOS Handler 在这些机制上叠加 DOS Packet 协议。

- **Signal**：适合事件位；仅凭位标志通常无法表示同类事件发生的次数。
- **MsgPort**：传递消息对象及其所有权/生命周期约定；通常不会自动深拷贝负载。
- **ReplyMsg**：发送者与接收者可以用回复来明确消息处理完成。
- **SMP**：必须使用跨核正确的锁和内存序；不能只靠 `Forbid()` 保护全局消息队列。

## 示例消息生命周期

```text
Sender                         Receiver
  |                               |
  | -- PutMsg(port, msg) -------->|
  |                               | GetMsg()
  |                               | process(msg)
  |<----------- ReplyMsg(msg) ----|
  | recover ownership             |
  | free(msg)                     |
```

**重要不变量：** `ReplyMsg()` 之后，接收方不再访问 `msg`；发送方收到回复之前不得释放或重用 `msg`。

## 建议内部结构（非原生 ABI）

```c
typedef enum { MSG_NEW, MSG_QUEUED, MSG_PROCESSING,
               MSG_REPLIED, MSG_CANCELLED } MsgState;

typedef struct {
    ListNode node;
    MsgPort *reply_port;
    uint64_t request_id;
    atomic_int state;
    uint32_t type, payload_size;
    void *payload;
} KernelMessage;

typedef struct {
    SpinLock queue_lock;
    List pending;
    Task *signal_target;
    uint32_t signal_mask;
    bool closing;
} MsgPort;
```

**实现关键点：**

1. `PutMsg` 在持锁期间入队，然后发布唤醒事件；避免“先唤醒后入队”的丢通知。
2. `GetMsg` 在锁内取出并改变状态。
3. Signal 以按位或记录待处理事件；如果事件需要计数，搭配消息队列或计数器。
4. `Wait` 睡眠前须原子检查待处理事件与待队列状态，以防丢失唤醒。
5. 关闭端口时要明确是拒收新消息、取消、还是等待队列清空。
6. 消息是否需要分配独立缓冲区、是否跨地址空间共享，是设计选择，不可隐式复制历史模型。

## Cancellation（建议扩展协议）

```text
NEW → QUEUED → PROCESSING → REPLIED
        └──────→ CANCELLED
                   ↑
              PROCESSING 可仅标记 cancel_requested，
              真正完成后才交回所有权
```

**取消请求**不一定意味着处理已经停止。必须等待完成/取消确认后才能释放请求和缓冲区。设备 I/O 进一步遵守 `AbortIO` + `WaitIO` 协议。

## 对象锁的使用

| 锁 | 用途 | 不能做什么 |
| --- | --- | --- |
| SpinLock | IRQ 路径、极短的队列修改 | 持锁睡眠/长时间 I/O |
| Mutex | 任务上下文的较长临界区 | 在不可睡眠 IRQ 上下文获取 |
| Semaphore | 共享资源数目/经典 Exec 协调 | 不能无协议防止 use-after-free |
| Atomics | 状态和计数 | 不能替代复杂结构的一致性协议 |

## 验收测试

- 10 万条消息收发不丢失、不重复回复。
- “消息已入队但尚未进入 Wait”不会永远阻塞。
- 取消与正常完成同时发生时只通知一次且只释放一次。
- 多个发送者和接收者在 SMP 压测下不破坏链表。
- `MsgPort` 销毁时不存在悬空回复目标。

## 参考

- [AmigaOS Exec Device I/O](https://wiki.amigaos.net/wiki/Exec_Device_I/O)
- [AmigaOS AmigaDOS Device I/O](https://wiki.amigaos.net/wiki/AmigaDOS_Device_Input_and_Output)
- [AROS DOS Library](https://developers.aros.org/documentation/app-dev/dos-library.html)
- [AROS Exec Autodocs（EXECSMP 注意事项）](https://developers.aros.org/documentation/autodocs/exec.html)
