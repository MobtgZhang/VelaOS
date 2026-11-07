# 09｜Graphics、RastPort、BitMap、Layers 与显示驱动

## 现有系统机制

经典 Amiga 图形栈分为多层：

- **graphics.library**：点、线、位图拷贝、文字、绘图模式。
- **BitMap / RastPort**：像素存储与绘图状态对象；经典图像可采用 bitplane（位平面）布局。
- **layers.library**：维护矩形叠放、裁剪、遮挡与重新暴露的区域。
- **intuition.library**：在 Layers 和 Graphics 之上管理窗口/屏幕。
- **AROS HIDD**：通过硬件无关接口适配不同显示设备；不应将其简单等同于某一个固定 GPU 驱动。

## 关键对象

| 对象 | 职责 |
| --- | --- |
| `BitMap` | 像素缓冲区（格式/位深/行跨度/plane） |
| `RastPort` | 绘制状态：当前笔、字体、光栅操作方式 |
| `View` / `ViewPort` | 经典显示组织概念 |
| `ColorMap` | 调色板与颜色映射 |
| `Layer` | 裁剪矩形与叠放顺序 |
| `Region` | 多个矩形构成的受损/裁剪区域 |
| `Screen` | Intuition 的显示上下文 |
| `Window` | Intuition 通过 Layer 实现的应用区域 |

## 推荐首版图形后端

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

## 基础操作（内部建议）

```c
typedef struct {
    uint32_t width, height, pitch;
    enum { PIXEL_RGBA8888, PIXEL_INDEX8, PIXEL_PLANAR } format;
    uint8_t *pixels;
} Surface;

void gfx_fill_rect(Surface *, Rect clip, Rect dst, uint32_t color);
void gfx_blit(Surface *src, Rect from, Surface *dst, Point to);
void gfx_draw_glyphs(Surface *, Font *, Point, const char *text);
void gfx_present(Rect *dirty, size_t count);
```

**注意**：函数名/结构来自本设计，不是经典 `graphics.library` 的实际定义。

## Layers：遮挡与损坏矩形

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

## 渲染与并发

- 对窗口绘制必须保证 Layer 的生命周期与剪裁范围稳定。
- 绘制与移动/关闭的竞争要有锁或命令队列。
- 一个窗口关闭后，不能再执行之前排队的绘图命令。
- 将 UI 的焦点管理放在 Intuition，而非 graphics.library。

## 验收

- 线、矩形、文字、BitMap blit 与透明区域计算正确。
- 三个窗口交错移动、最小化、重叠不会留下脏像素。
- 超出 bounds 的绘制完全被裁剪。
- 同一文档在不同像素跨度（pitch）下无越界。
- 调色板/bitplane 转换可以通过固定图片回归测试。

## 参考

- [AmigaOS Graphics Primitives](https://wiki.amigaos.net/wiki/Graphics_Primitives)
- [AmigaOS Layers Library](https://wiki.amigaos.net/wiki/Layers_Library)
- [AmigaOS Intuition Screens](https://wiki.amigaos.net/wiki/Intuition_Screens)
- [AROS FAQ — HIDD](https://www.aros.org/documentation/faq.html)
