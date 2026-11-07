# 11｜键盘、鼠标、input.device、Commodities 与输入焦点

本报告定义 VelaOS 输入子系统的完整设计，覆盖 raw input driver、keymap、input.device 事件流、Commodities 过滤/转换、Intuition IDCMP 分发，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

经典 AmigaOS 的 `input.device` 是输入事件流的汇聚/分发层，可收集 keyboard、gameport、timer 等来源的事件；Intuition 和 console.device 消费这些事件。`InputEvent` 具有事件类别、键码、修饰键（qualifier）、位置和时间戳。

`commodities.library` 提供全局热键及输入过滤/转换框架，Broker、Filter、Sender、Signal、Translate 等对象组合形成事件处理流程。**应用一般应通过 IDCMP 或 console 读取自己的输入，而非直接抢占低层输入设备。**

---

## 2. 推荐链路

```text
PS/2 / USB HID / VirtIO input / BT controller
              │
        raw input drivers
              │
        Keymap / Mouse
              │
     input.device (ordered InputEvent stream)
              │
     Commodities filtering (optional)
              │
        Intuition dispatcher
              │
      active Window.UserPort
```

**输入源注册**应与事件分发分开：来源可能发生变化，但 UI 层不必理解 USB 报告格式。

---

## 3. 事件内部定义（自拟）

```c
typedef enum {
    INPUT_KEY_DOWN,
    INPUT_KEY_UP,
    INPUT_MOUSE_MOVE,
    INPUT_MOUSE_BUTTON,
    INPUT_MOUSE_WHEEL,
    INPUT_TEXT_INPUT,        /* 已通过 keymap 转换后的 Unicode 文本 */
    INPUT_TIMER,             /* 定时器事件（双击间隔、重复等） */
} RawInputType;

typedef struct {
    RawInputType type;
    uint64_t     monotonic_ns;   /* 单调时钟时间戳 */
    uint32_t     device_id;      /* 输入设备唯一 id */
    uint32_t     keycode;        /* 原始扫描码 / 按钮码 */
    uint32_t     modifiers;      /* Shift/Control/Alt/Command 掩码 */
    int32_t      x, y;           /* 绝对位置（鼠标） */
    int32_t      dx, dy;         /* 相对位移（鼠标移动/滚轮） */
    uint32_t     unicode;        /* 已转换的 Unicode 码点 */
} RawInputEvent;
```

**注意：** 这不是系统原生 `InputEvent` 的二进制布局；它可以用于底层输入抽象，之后再转换成兼容的 InputEvent/IDCMP。

---

## 4. 输入设备驱动

### 键盘（PS/2 / USB HID）

- 原始扫描码（scancode）通过中断或轮询读取。
- 按键按下产生 `INPUT_KEY_DOWN`，释放产生 `INPUT_KEY_UP`。
- 修饰键（Shift/Ctrl/Alt/Command）状态由 keymap 层维护。
- 自动重复：按住键超过阈值后周期性产生 `INPUT_KEY_DOWN`，重复率可配置。
- NumLock/CapsLock/ScrollLock 状态切换，反映在 LED（如有）。

### 鼠标（PS/2 / USB HID / VirtIO）

- 鼠标移动产生 `INPUT_MOUSE_MOVE`，带 dx/dy 相对位移。
- 按钮按下/释放产生 `INPUT_MOUSE_BUTTON`。
- 滚轮产生 `INPUT_MOUSE_WHEEL`。
- 鼠标绝对位置由 input.device 根据 dx/dy 累加，限制在屏幕范围内。

### 其他设备

- 游戏手柄（gameport）：按钮和轴映射为标准输入事件。
- 定时器：用于双击检测、键盘重复、光标闪烁。

---

## 5. input.device 事件分发

input.device 是单一有序事件流的所有者：

1. 各 raw input driver 将事件 Push 到 input.device 队列。
2. input.device 按时间戳排序，合并同一设备的连续事件（如鼠标移动 burst）。
3. 注册的 handler（Intuition、Commodities、console）按优先级顺序接收事件。
4. Handler 可消费事件（不再向下传递）或允许继续传递。

**队列容量**：至少 256 个事件，溢出时丢弃最旧事件并设置溢出标志。

---

## 6. 事件处理原则

1. **保留原始键码**，并在独立 keymap 层生成文本，不要把 `KEY_DOWN` 直接等价于字符。
2. 区分“按键事件”和“文本输入”（组合键、输入法、死键等）。
3. 鼠标移动与按钮状态应维持一致序列。
4. 过滤器不应无故吞掉所有事件。
5. 捕获鼠标/全局热键需要释放机制，程序退出时自动撤销。
6. GUI 焦点丢失时，为按住的键/鼠标按钮提供一致的状态修正策略（全部发 KEY_UP）。
7. 多设备输入分别保存 device_id，避免键鼠状态串扰。

---

## 7. Keymap 层

keymap 将原始扫描码转换为文本字符：

- 普通按键：扫描码 + 修饰键 → Unicode 码点。
- Shift 状态：大写字母、符号键上层字符。
- AltGr/Control 组合：特殊字符。
- 死键（dead key）：按下后不立即输出，与下一个按键组合（如 `´` + `e` → `é`）。
- 输入法（IME）：第二阶段支持，首版可跳过。

---

## 8. Commodities（第二阶段可选）

Commodities 提供全局热键及输入过滤/转换：

| 对象 | 职责 |
| --- | --- |
| Broker | 商品注册/注销，与 input.device 绑定 |
| Filter | 对输入事件进行条件过滤（如仅当某窗口激活时） |
| Sender | 向指定端口发送信号或消息 |
| Signal | 向任务发送 Exec 信号位 |
| Translate | 修改事件内容（如映射按键） |

商品组合形成事件处理流程：输入 → Filter → Translate → Sender/Signal。

首版可不实现 Commodities，直接由 Intuition 处理全局热键（如关闭窗口、切换焦点）。

---

## 9. console.device

console.device 把当前交互转成文本输入输出：

- 读取：从 input.device 获取 KEY_DOWN/TEXT_INPUT，返回字节给 Shell。
- 写入：将字节输出到当前活动窗口或串口。
- 行编辑：Backspace 删除字符，Enter 提交行（可选，Shell 也可自己处理）。
- 只有一个活动 console，绑定到当前焦点窗口。

---

## 10. Intuition 输入分发

Intuition 从 input.device 接收事件，根据命中测试（hit-test）分发：

1. 鼠标事件：根据坐标确定目标窗口和 Gadget。
2. 键盘事件：发送给当前焦点窗口的 UserPort。
3. 事件类别映射：
   - `INPUT_KEY_DOWN/UP` → `IDCMP_RAWKEY`
   - `INPUT_MOUSE_MOVE` → `IDCMP_MOUSEMOVE`
   - `INPUT_MOUSE_BUTTON` → `IDCMP_MOUSEBUTTONS`
   - `INPUT_MOUSE_WHEEL` → `IDCMP_MOUSEWHEEL`
   - `INPUT_TEXT_INPUT` → `IDCMP_INTUITIME` 或自定义类

---

## 11. 最小兼容功能

- keyboard.device：键码输入与按键释放。
- mouse/gameport 兼容层：鼠标移动、左右键、滚轮（扩展）。
- input.device：有序事件、可注册 handler、修饰键状态。
- console.device：把当前交互转成文本输入输出。
- Intuition：活动窗口接收对应 `IDCMP_RAWKEY` / `IDCMP_MOUSEBUTTONS` 等。
- commodities：第二阶段可选，全局热键和过滤链。

---

## 12. 验收

- 在两个窗口之间切换输入焦点，不混入键盘事件。
- 连续按键、自动重复、修饰键释放状态一致。
- 1000Hz 鼠标移动不会溢出队列造成死锁。
- 程序退出后其全局快捷键、过滤器和 capture 都被清理。
- USB 键盘热拔出不会遗留“永远按住”的按键状态（发送所有按键的 KEY_UP）。
- 双击检测：同一位置两次点击间隔小于阈值（如 500ms）判定为双击。
- 修饰键组合（如 Ctrl+C）正确识别并传递。

---

## 13. 参考

- [AmigaOS Input Device](https://wiki.amigaos.net/wiki/Input_Device)
- [AmigaOS Commodities Exchange Library](https://wiki.amigaos.net/wiki/Commodities_Exchange_Library)
- [AmigaOS Window Communication](https://wiki.amigaos.net/wiki/Window_Communication)
