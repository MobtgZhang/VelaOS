# 09｜Graphics、RastPort、BitMap、Layers 与显示驱动

本报告定义 VelaOS 图形栈的完整设计，覆盖 graphics.library 语义、RastPort 绘图状态、BitMap 像素缓冲、Layers 裁剪与叠放、软件光栅化、显示驱动接口，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

经典 Amiga 图形栈分为多层：

| 层级 | 职责 |
| --- | --- |
| **graphics.library** | 点、线、矩形、位图拷贝（Blit）、文字、绘图模式 |
| **BitMap / RastPort** | 像素存储与绘图状态对象；经典图像可采用 bitplane（位平面）布局 |
| **layers.library** | 维护矩形叠放、裁剪、遮挡与重新暴露的区域（Damage Region） |
| **intuition.library** | 在 Layers 和 Graphics 之上管理窗口/屏幕 |
| **AROS HIDD** | 通过硬件无关接口适配不同显示设备；不应将其简单等同于某一个固定 GPU 驱动 |

VelaOS 首版采用 **RGBA8888 软件合成** 后端，内部 chunky 像素格式，外部保留 Amiga 风格 API 语义。不急着做 GPU 加速。

---

## 2. 关键对象

| 对象 | 职责 |
| --- | --- |
| `BitMap` | 像素缓冲区（格式/位深/行跨度/plane 指针） |
| `RastPort` | 绘制状态：当前笔（APen/BPen）、字体、光栅操作模式、剪裁矩形 |
| `View` / `ViewPort` | 经典显示组织概念（VelaOS 可简化为 Screen 直接持有 BitMap） |
| `ColorMap` | 调色板与颜色映射（256 条目，每条目 RGB） |
| `Layer` | 裁剪矩形与叠放顺序（z-order）、损坏区域（Damage Region） |
| `Region` | 多个矩形构成的受损/裁剪区域 |
| `Screen` | Intuition 的显示上下文，持有全屏 BitMap 和调色板 |
| `Window` | Intuition 通过 Layer 实现的应用区域，持有自己的 RastPort |

---

## 3. 推荐首版图形后端

**不急着做 GPU 加速，先支持 RGBA8888 软件合成**：

```text
RastPort commands (SetAPen, RectFill, Text, BltBitMap ...)
           │
      Software Rasterizer
           │
    clip to Layer / Region
           │
        Offscreen BitMap
           │
     Compositor / damage list
           │
      Framebuffer present
```

兼容层需要将 bitplane/调色板的逻辑转成 RGBA；内部可以采用 chunky 像素格式，外部保留经检验的 Amiga API 语义。

---

## 4. 基础操作（内部建议）

```c
typedef enum {
    PIXEL_RGBA8888,   /* chunky，每像素 4 字节 */
    PIXEL_INDEX8,     /* 调色板索引 */
    PIXEL_PLANAR      /* 经典 Amiga bitplane（兼容层转换用） */
} PixelFormat;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t pitch;     /* 每行字节数，可大于 width * 4 */
    PixelFormat format;
    uint8_t *pixels;    /* 位于可访问内存（帧缓冲或离屏） */
} Surface;

typedef struct {
    Surface *surface;   /* 所属位图 */
    int16_t  cp_x, cp_y;/* 当前绘图位置（Current Position） */
    uint32_t a_pen;     /* 前景笔索引 */
    uint32_t b_pen;     /* 背景笔索引 */
    uint32_t draw_mode; /* JAM1/JAM2/COMPLEMENT/INVERSVID 等 */
    Rect     clip_rect; /* 剪裁范围 */
    Font    *font;      /* 当前字体 */
} RastPort;

/* 基础绘图原语 */
void gfx_fill_rect(Surface *surf, const Rect *dst, uint32_t color);
void gfx_blit(Surface *src, const Rect *from, Surface *dst, int16_t dx, int16_t dy);
void gfx_draw_glyphs(Surface *surf, Font *font, int16_t x, int16_t y, const char *text);
void gfx_present(const Rect *dirty, uint32_t count);
void gfx_set_clip(RastPort *rp, const Rect *clip);
void gfx_set_pen(RastPort *rp, uint32_t pen, int is_fg);
```

**注意**：函数名/结构来自本设计，不是经典 `graphics.library` 的实际定义。经典 API 的头文件和版本对应关系以 SDK 为准。

---

## 5. 笔与颜色

- `APen`（前景笔）用于线条、文字、矩形边框。
- `BPen`（背景笔）用于填充、渐变底色。
- 笔索引映射到 `ColorMap` 中的真实 RGB 值。
- `SetAPen` / `SetBPen` 更改 RastPort 状态，不立即绘图。
- `RGB` 颜色格式为 `0x00RRGGBB`。

---

## 6. Layers：遮挡与损坏矩形

窗口从 A 移动到 B：

1. 计算旧区域以及新区域。
2. 把暴露出来的区域加入后方 Layer 的 damage region。
3. 将移动后的窗口区域标记为脏区域。
4. 依据 z-order 从后往前重绘被影响区域。
5. 最终仅向显示后端刷新 dirty rectangles。

```text
Window A (back)   +--------------------------+
                  |  [overlap hidden]       |
Window B (front)  |     +-----------+        |
                  |     |  B        |        |
                  +-----+-----------+--------+
```

**常见错误**：窗口右移时只绘制新位置、不修复旧位置，产生拖影。

### Layer 内部状态

```c
typedef struct Layer {
    uint32_t id;
    Rect     bounds;        /* 在屏幕坐标系中的矩形 */
    uint32_t z_order;       /* 越大越靠前 */
    Surface  *backing;      /* 离屏位图，可为 NULL（直接绘制到屏幕） */
    Region   *damage;       /* 脏矩形集合 */
    RastPort *rp;           /* 该 Layer 的绘图状态 */
    struct Layer *prev;     /* z-order 链表 */
    struct Layer *next;
} Layer;
```

---

## 7. 渲染与并发

- 对窗口绘制必须保证 Layer 的生命周期与剪裁范围稳定。
- 绘制与移动/关闭的竞争要有锁或命令队列。
- 一个窗口关闭后，不能再执行之前排队的绘图命令。
- 将 UI 的焦点管理放在 Intuition，而非 graphics.library。
- 绘图命令应幂等：重复执行相同命令产生相同结果。

---

## 8. 字体与文本渲染

- 位图字体（bitmap font）作为首选，每个字符一个固定大小的 glyph 位图。
- 支持 `Text()`：在当前位置绘制 NUL 终止字符串，返回前进宽度。
- 字符间距（kerning）可选，首版可固定等宽。
- 文字剪裁应遵循 RastPort 的 clip 矩形。

---

## 9. 显示驱动接口

```c
typedef struct {
    int  (*init)(void);
    void (*set_mode)(uint32_t width, uint32_t height, uint32_t pitch);
    void (*present)(const Rect *dirty, uint32_t count);  /* 刷新脏区域 */
    uint8_t *(*get_framebuffer)(void);                  /* 返回帧缓冲地址 */
    void (*set_palette)(uint32_t start, uint32_t count, const uint32_t *rgb);
    void (*shutdown)(void);
} DisplayDriver;

int display_register(DisplayDriver *drv, const char *name);
```

首版驱动：直接写入 VGA/LFB 帧缓冲（QEMU GOP/UEFI 帧缓冲），软件合成后 compositing 完成后调用 `present`。

---

## 10. 双缓冲与撕裂

- 如果硬件支持，使用双缓冲：后台缓冲区绘制完成后一次性 `present` 到前台。
- 软件实现：绘制到离屏 BitMap，`present` 时执行 `memcpy` 到可见帧缓冲，仅拷贝 dirty rectangles。
- 避免在扫描线过程中部分刷新导致画面撕裂（首版可接受轻微撕裂，后续加 VBlank 同步）。

---

## 11. 验收

- 线、矩形、文字、BitMap blit 与透明区域计算正确。
- 三个窗口交错移动、最小化、重叠不会留下脏像素。
- 超出 bounds 的绘制完全被裁剪，不越界写入。
- 同一文档在不同像素跨度（pitch）下无越界：`row * pitch + col` 均在缓冲区范围内。
- 调色板/bitplane 转换可以通过固定图片回归测试。
- 快速连续移动窗口不导致图形状态损坏或 Layer 链表断裂。

---

## 12. 参考

- [AmigaOS Graphics Primitives](https://wiki.amigaos.net/wiki/Graphics_Primitives)
- [AmigaOS Layers Library](https://wiki.amigaos.net/wiki/Layers_Library)
- [AmigaOS Intuition Screens](https://wiki.amigaos.net/wiki/Intuition_Screens)
- [AROS FAQ — HIDD](https://www.aros.org/documentation/faq.html)
