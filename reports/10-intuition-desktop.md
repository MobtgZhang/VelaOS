# 10｜Intuition 窗口系统、BOOPSI、Zune 与桌面

本报告定义 VelaOS GUI 子系统的完整设计，覆盖 Intuition 窗口/屏幕管理、IDCMP 事件、BOOPSI 对象模型、Zune/MUI 风格布局、桌面/Workbench 实现，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

**Intuition 不是 AmigaOS 整个内核，而是 GUI 子系统。**

| 组件 | 职责 |
| --- | --- |
| `Intuition` | Screen、Window、Menu、Gadget、Requester 和输入事件 |
| `IDCMP` | Intuition Direct Communications Message Port，应用通过 `Window.UserPort` 接收 `IntuiMessage` |
| `BOOPSI` | C 语言对象系统，基于 Class/Method/Attribute，常用 `TagItem` 构造参数 |
| `GadTools/ASL` | 常用控件与文件/字体/颜色请求器等 |
| `Workbench` | AmigaOS 桌面及图标文件管理器 |
| `Wanderer` | AROS 典型桌面；`Zune` 是其 MUI 风格的对象式 GUI 框架，构建在 BOOPSI 上 |

---

## 2. 窗口与事件结构

```text
keyboard / mouse
        │
   input.device
        │
 Intuition input dispatch
        │             focus/window hit-test
        ├─────────────────────────────┐
        ▼                             ▼
   Window A.UserPort             Window B.UserPort
        │                             │
 IDCMP messages                  IDCMP messages
        │                             │
   App A event loop            App B event loop
```

UI 事件通常仅送至目标窗口，不能直接广播给所有应用，否则焦点/键盘输入会混乱。

### IntuiMessage 结构（概略）

```c
typedef struct {
    uint32_t class;       /* IDCMP_*：CLOSEWINDOW, REFRESHWINDOW, GADGETUP, ... */
    uint16_t code;        /* 附加代码：按键码、 Gadget id 等 */
    uint16_t qualifier;   /* 修饰键：Shift/Control/Alt */
    int16_t  mouse_x, mouse_y;
    uint32_t seconds, micros;  /* 时间戳 */
    void    *i_address;   /* 触发事件的 Gadget/Window 指针 */
    uint32_t special;     /* 特殊信息 */
} IntuiMessage;
```

### 典型应用循环（Amiga 风格伪代码）

```c
while (!done) {
    Wait(window_signal_mask);
    while ((msg = GetMsg(window->UserPort)) != NULL) {
        switch (msg->Class) {
            case IDCMP_CLOSEWINDOW:   done = true; break;
            case IDCMP_REFRESHWINDOW: redraw_window(window); break;
            case IDCMP_GADGETUP:      handle_gadget(msg); break;
            case IDCMP_RAWKEY:        handle_key(msg); break;
            case IDCMP_MOUSEBUTTONS:  handle_mouse(msg); break;
            default: break;
        }
        ReplyMsg((struct Message *)msg);  /* 回复后不得访问 msg */
    }
}
```

收到消息后先读取需要的信息，再回复；回复后不得访问消息内容。

---

## 3. 建议内部对象

```text
Display
  └─ Screen (mode, palette, windows)
       ├─ Window (z, rect, focus, clip, port)
       │    ├─ Gadget tree / menus
       │    └─ IDCMP port
       └─ Window ...

WindowManager
  - 窗口创建/关闭、焦点、z-order、移动/缩放
EventDispatcher
  - 坐标命中测试、快捷键、菜单模式
LayoutEngine
  - Gadget 自动布局（第二阶段）
Desktop
  - 基于 DOS 枚举文件并读取图标，不直接接管 WindowManager
RequesterService
  - 通用文件选择/确认对话框
```

### Window 内部状态

```c
typedef struct Window {
    uint32_t id;
    Rect     bounds;           /* 在屏幕坐标系中的矩形 */
    Rect     inner;            /* 内部绘图区域（扣除标题栏/边框） */
    uint32_t z_order;
    char     title[128];
    uint32_t flags;            /* WINDOW_CLOSE_GADGET, WINDOW_DRAGGABLE, ... */
    Port     user_port;        /* IDCMP 消息端口 */
    uint32_t idcmp_mask;       /* 感兴趣的事件类别掩码 */
    Layer   *layer;            /* 关联的 graphics Layer */
    RastPort *rp;              /* 绘图状态 */
    Gadget  *gadget_head;      /* Gadget 链表 */
    Menu    *menu_head;        /* 菜单链表 */
    bool     focused;
    bool     visible;
    bool     active;           /* 是否为焦点窗口 */
    struct Window *prev;
    struct Window *next;
} Window;
```

### Screen 内部状态

```c
typedef struct Screen {
    uint32_t id;
    Rect     bounds;           /* 全屏矩形 */
    uint32_t depth;            /* 位深 */
    uint32_t *palette;         /* 调色板，每条目 0x00RRGGBB */
    Surface  *bitmap;          /* 全屏位图 */
    Layer   *backdrop;         /* 底层 Layer（桌面背景） */
    Window  *window_list;      /* z-order 链表头 */
    Window  *focus_window;     /* 当前焦点窗口 */
} Screen;
```

---

## 4. BOOPSI / Zune 的实现建议

**第一阶段**只做五个概念：RootClass、对象创建/销毁、属性设置/获取、方法分派、TagList。

```c
/* BOOPSI 最小对象模型 */
typedef struct Class Class;
typedef struct Object Object;

typedef uint32_t (*DispatchFunc)(Class *cl, Object *obj, uint32_t method, uint32_t arg1, uint32_t arg2);

struct Class {
    const char *name;
    Class      *parent;
    DispatchFunc dispatch;
    uint32_t    instance_size;
};

struct Object {
    Class *class;
    /* 实例数据紧跟在对象头之后 */
};

uint32_t DoMethod(Object *obj, uint32_t method, uint32_t arg1, uint32_t arg2);
uint32_t SetAttrs(Object *obj, ...);   /* TagItem 列表 */
uint32_t GetAttr(Object *obj, uint32_t attr_id, uint32_t *storage);
Object  *NewObject(Class *cl, ...);    /* TagItem 列表 */
void     DisposeObject(Object *obj);
```

**第二阶段**再做：

- GadgetClass：按下、松开、禁用状态、重绘。
- ImageClass：显示图像对象。
- Zune/MUI 风格的布局容器、属性通知与 theme。
- 响应式布局与字体尺寸变化。

不要一开始就实现全部 MUI/Zune API；最小用户界面只需要按钮、列表、文本框和菜单。

---

## 5. 桌面的最小实现

1. 打开一个 Screen（640×480 或 1280×800，RGBA8888）。
2. 枚举 `SYS:` 文件，显示列表或网格。
3. 识别 `.info` 图标，读不到时使用默认图标。
4. 双击目录进入；双击程序建立 DOS Process。
5. 双击文档时调用 DefaultTool / 文件类型映射。
6. 保存窗口坐标与图标布局（Snapshot 类行为）。
7. 提供“丢弃到垃圾桶”、“重命名”、“新建文件夹”等基础操作。

桌面本身是一个普通 Intuition 应用，通过 `Workbench.library` 注册 AppIcon 和 AppWindow。

---

## 6. 输入焦点与 z-order

- 点击窗口将其提至 z-order 顶端，并设为焦点窗口。
- 焦点窗口的 UserPort 接收所有键盘 IDCMP 消息。
- 非焦点窗口的鼠标点击先传递给该窗口（可能改变焦点），不丢事件。
- 关闭焦点窗口时，焦点自动转移到 z-order 下一个可见窗口；若无则焦点为空。
- 窗口移动/缩放时，损坏区域加入后方 Layer 的 damage region。

---

## 7. Requester 与对话框

- Requester 是模态对话框，阻塞父窗口输入，但不阻塞其他窗口。
- 文件选择 Requester：枚举目录、显示文件列表、确认/取消。
- 确认 Requester：显示消息、确认按钮。
- Requester 关闭后自动释放，不影响父窗口状态。

---

## 8. 验收

- 可同时打开 5 个窗口并正确维护焦点与 z-order。
- 键盘输入只送到活动窗口，不广播给所有应用。
- 关闭窗口时不会将 `IntuiMessage` 发给已经释放的端口。
- 点选一个 Gadget 产生正确 DOWN/UP 事件，并回复消息。
- 拖动窗口时损坏区域刷新正确，不留拖影。
- 桌面可从图标启动一个简单程序，并能接收其退出状态。
- Requester 模态阻塞父窗口，关闭后父窗口恢复输入。

---

## 9. 参考

- [AmigaOS Intuition Library](https://wiki.amigaos.net/wiki/Intuition_Library)
- [AmigaOS Window Communication](https://wiki.amigaos.net/wiki/Window_Communication)
- [AmigaOS Workbench Library](https://wiki.amigaos.net/wiki/Workbench_Library)
- [AROS Zune Application Development](https://developers.aros.org/documentation/zune-dev/zune-application-development.html)
