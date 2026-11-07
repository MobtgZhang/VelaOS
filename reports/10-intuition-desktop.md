# 10｜Intuition 窗口系统、BOOPSI、Zune 与桌面

## 现有系统机制

**Intuition 不是 AmigaOS 整个内核，而是 GUI 子系统。**

- `Intuition`：Screen、Window、Menu、Gadget、Requester 和输入事件。
- `IDCMP`：Intuition Direct Communications Message Port，应用通过 `Window.UserPort` 接收 `IntuiMessage`。
- `BOOPSI`：C 语言对象系统，基于 Class/Method/Attribute，常用 `TagItem` 构造参数。
- `GadTools/ASL`：常用控件与文件/字体/颜色请求器等。
- `Workbench`：AmigaOS 桌面及图标文件管理器。
- `Wanderer`：AROS 典型桌面；`Zune` 是其 MUI 风格的对象式 GUI 框架，构建在 BOOPSI 上。

## 窗口与事件结构

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
 App A event loop                App B event loop
```

UI 事件通常仅送至目标窗口，不能直接广播给所有应用，否则焦点/键盘输入会混乱。

### 典型应用循环（Amiga 风格伪代码）

```c
while (!done) {
    Wait(window_signal_mask);
    while ((msg = GetMsg(window->UserPort)) != NULL) {
        switch (msg->Class) {
            case IDCMP_CLOSEWINDOW: done = true; break;
            case IDCMP_REFRESHWINDOW: redraw_window(window); break;
            case IDCMP_GADGETUP: handle_gadget(msg); break;
            default: break;
        }
        ReplyMsg((struct Message *)msg);
    }
}
```

实际 API 的头文件和版本对应关系以 SDK 为准。收到消息后先读取需要的信息，再回复；回复后不得访问消息内容。

## 建议内部对象

```text
Display
  └─ Screen (mode, palette, windows)
       ├─ Window (z, rect, focus, clip, port)
       │    └─ Gadget tree / menus
       └─ Window ...
```

- `WindowManager`：窗口创建/关闭、焦点、z-order、移动/缩放。
- `EventDispatcher`：坐标命中测试、快捷键、菜单模式。
- `LayoutEngine`：Gadget 自动布局（第二阶段）。
- `Desktop`：基于 DOS 枚举文件并读取图标，不直接接管 WindowManager。
- `RequesterService`：通用文件选择/确认对话框。

## BOOPSI / Zune 的实现建议

**第一阶段**只做五个概念：RootClass、对象创建/销毁、属性设置/获取、方法分派、TagList。

**第二阶段**再做：

- GadgetClass：按下、松开、禁用状态、重绘。
- ImageClass：显示图像对象。
- Zune/MUI 风格的布局容器、属性通知与 theme。
- 响应式布局与字体尺寸变化。

不要一开始就实现全部 MUI/Zune API；最小用户界面只需要按钮、列表、文本框和菜单。

## 桌面的最小实现

1. 打开一个 Screen。
2. 枚举 `SYS:` 文件，显示列表或网格。
3. 识别 `.info` 图标，读不到时使用默认图标。
4. 双击目录进入；双击程序建立 DOS Process。
5. 双击文档时调用 DefaultTool / 文件类型映射。
6. 保存窗口坐标与图标布局（Snapshot 类行为）。

## 验收

- 可同时打开 5 个窗口并正确维护焦点与 z-order。
- 键盘输入只送到活动窗口。
- 关闭窗口时不会将 `IntuiMessage` 发给已经释放的端口。
- 点选一个 Gadget 产生正确 DOWN/UP 事件。
- 拖动窗口时损坏区域刷新正确。
- 桌面可从图标启动一个简单程序，并能接收其退出状态。

## 参考

- [AmigaOS Intuition Library](https://wiki.amigaos.net/wiki/Intuition_Library)
- [AmigaOS Window Communication](https://wiki.amigaos.net/wiki/Window_Communication)
- [AmigaOS Workbench Library](https://wiki.amigaos.net/wiki/Workbench_Library)
- [AROS Zune Application Development](https://developers.aros.org/documentation/zune-dev/zune-application-development.html)
