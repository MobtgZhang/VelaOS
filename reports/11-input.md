# 11｜键盘、鼠标、input.device、Commodities 与输入焦点

## 现有系统机制

经典 AmigaOS 的 `input.device` 是输入事件流的汇聚/分发层，可收集 keyboard、gameport、timer 等来源的事件；Intuition 和 console.device 消费这些事件。`InputEvent` 具有事件类别、键码、修饰键（qualifier）、位置和时间戳。

`commodities.library` 提供全局热键及输入过滤/转换框架，Broker、Filter、Sender、Signal、Translate 等对象组合形成事件处理流程。**应用一般应通过 IDCMP 或 console 读取自己的输入，而非直接抢占低层输入设备。**

## 推荐链路

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

## 事件内部定义（自拟）

```c
typedef struct {
    enum { KEY_DOWN, KEY_UP, MOUSE_MOVE,
           MOUSE_BUTTON, MOUSE_WHEEL, TEXT_INPUT } type;
    uint64_t monotonic_ns;
    uint32_t device_id;
    uint32_t keycode, modifiers;
    int32_t x, y, dx, dy;
    uint32_t unicode;
} RawInputEvent;
```

**注意：** 这不是系统原生 `InputEvent` 的二进制布局；它可以用于底层输入抽象，之后再转换成兼容的 InputEvent/IDCMP。

## 事件处理原则

1. **保留原始键码**，并在独立 keymap 层生成文本，不要把 `KEY_DOWN` 直接等价于字符。
2. 区分“按键事件”和“文本输入”（组合键、输入法、死键等）。
3. 鼠标移动与按钮状态应维持一致序列。
4. 过滤器不应无故吞掉所有事件。
5. 捕获鼠标/全局热键需要释放机制，程序退出时自动撤销。
6. GUI 焦点丢失时，为按住的键/鼠标按钮提供一致的状态修正策略。
7. 多设备输入分别保存 device_id，避免键鼠状态串扰。

## 最小兼容功能

- keyboard.device：键码输入与按键释放。
- mouse/gameport 兼容层：鼠标移动、左右键、滚轮（扩展）。
- input.device：有序事件、可注册 handler、修饰键状态。
- console.device：把当前交互转成文本输入输出。
- Intuition：活动窗口接收对应 `IDCMP_RAWKEY` / `IDCMP_MOUSEBUTTONS` 等。
- commodities：第二阶段可选，全局热键和过滤链。

## 验收

- 在两个窗口之间切换输入焦点，不混入键盘事件。
- 连续按键、自动重复、修饰键释放状态一致。
- 1000Hz 鼠标移动不会溢出队列造成死锁。
- 程序退出后其全局快捷键、过滤器和 capture 都被清理。
- USB 键盘热拔出不会遗留“永远按住”的按键状态。

## 参考

- [AmigaOS Input Device](https://wiki.amigaos.net/wiki/Input_Device)
- [AmigaOS Commodities Exchange Library](https://wiki.amigaos.net/wiki/Commodities_Exchange_Library)
- [AmigaOS Window Communication](https://wiki.amigaos.net/wiki/Window_Communication)
