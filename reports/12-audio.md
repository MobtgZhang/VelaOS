# 12｜音频设备、AHI、PCM 混音与实时调度

本报告定义 VelaOS 音频栈的完整设计，覆盖 AHI 抽象、PCM 混音器、设备后端、DMA 环形缓冲、实时调度约束，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

经典 Amiga 机器的音频有 Paula 等硬件路径及 `audio.device`；更通用的高层音频抽象是 **AHI**，目标包括硬件无关、多个应用共享音频设备以及录制/播放能力。不同 AmigaOS、AROS 发行版的实际声卡驱动和 AHI 支持不应一概而论。

VelaOS 首版采用软件混音 + null 后端，随后支持 QEMU 可仿真的声卡后端，Paula/audio.device 作为后续经典兼容目标。

---

## 2. 推荐音频栈

```text
Game / Media App / Sound Datatype
                │
             AHI API
                │
        Audio Mixer Service
        ├── stream mixing
        ├── resampling
        ├── channel mapping
        └── limiter / volume
                │
          Device backend
      (VirtIO / HDA / Paula / null)
                │
         DMA ring buffer
                │
           Audio Hardware
```

---

## 3. 最小能力

### 第一步：null/sink 后端

即使没有声卡，也实现写 PCM 到环形缓冲区的 null/sink 后端，保证端到端 API 可测试。null 后端接受所有数据但不实际输出，仅统计 underrun 和写入帧数。

### 第二步：QEMU 声卡后端

使用 QEMU 可仿真的声卡后端，支持：
- 16-bit signed PCM
- 立体声（2 channels）
- 48kHz 采样率
- DMA 环形缓冲区

### 第三步：多应用混音

两个应用同时提交音频数据，能在混音器中合成，独立音量控制。

---

## 4. 音频流结构（示意）

```c
typedef struct {
    uint32_t  sample_rate;   /* 44100 / 48000 */
    uint16_t  channels;      /* 1 / 2 */
    uint16_t  format;        /* PCM_S16_LE / PCM_F32 / PCM_U8 */
    RingBuffer pcm;          /* 待混音的 PCM 数据 */
    atomic_uint underruns;   /* 欠载计数 */
    uint32_t   volume_q16;   /* 音量，Q16 固定点，0x10000 = 1.0 */
    bool        paused;      /* 暂停标志 */
    bool        draining;    /* 排空中 */
    void       *backend_private; /* 后端私有数据 */
} AudioStream;
```

---

## 5. 混音器（Mixer Service）

混音器在专用音频线程中运行：

1. 收集所有活跃 stream 的 PCM 数据。
2. 按各自音量（volume_q16）缩放。
3. 混合到单个输出缓冲区。
4. 可选重采样：若 stream 采样率 != 设备采样率，进行线性插值重采样。
5. 应用 limiter（防止削波）和主音量。
6. 写入设备 DMA 环形缓冲区。

**混音公式**（每个样本）：

```
out = clamp(sum(stream[i].sample * stream[i].volume_q16 / 0x10000), -1.0, 1.0)
```

---

## 6. 设备后端与 DMA

### 环形缓冲区

```c
typedef struct {
    void     *buf;           /* DMA 可访问内存 */
    uint32_t size_bytes;     /* 缓冲区总大小 */
    uint32_t write_pos;      /* 混音器写位置 */
    uint32_t read_pos;       /* 硬件读位置（由中断或轮询更新） */
    uint32_t period_size;    /* 中断周期大小 */
} DmaRingBuffer;
```

### 后端接口

```c
typedef struct {
    int  (*open)(uint32_t sample_rate, uint16_t channels, uint16_t format);
    void (*close)(void);
    int  (*write)(const void *pcm, uint32_t frames);   /* 写入 DMA 环形缓冲 */
    uint32_t (*get_write_pos)(void);                    /* 返回可写帧数 */
    int  (*set_volume)(uint32_t q16);                   /* 主音量 */
    int  (*start)(void);                                /* 启动 DMA 播放 */
    int  (*stop)(void);                                 /* 停止 DMA 播放 */
    void (*get_stats)(AudioStats *);                    /* underrun, overrun 等 */
} AudioBackend;

int audio_register_backend(AudioBackend *backend, const char *name);
```

---

## 7. 音频线程注意事项

- 尽可能避免在音频实时路径分配内存：所有缓冲区在启动时预分配。
- 避免等待文件锁、窗口锁和其他高延迟系统服务：音频线程不应阻塞。
- 流生命周期须防止“移除 stream 时 DMA 回调仍使用它”：使用 RCU 或引用计数保护。
- 设备环形缓冲区需要明确写指针、读指针和缓存一致性：DMA 写前刷新缓存，读后失效缓存。
- 提供欠载（underrun）计数器和可观察日志：underrun 时输出静音，不中断。
- 建议音频 Task 使用适当优先级，但不能永远饿死其他 Task。

---

## 8. 推荐 API（设计方案）

```c
AudioStreamId audio_open(uint32_t sample_rate, uint16_t channels, uint16_t sample_format);
size_t  audio_write(AudioStreamId id, const void *pcm, size_t frames);
void    audio_set_volume(AudioStreamId id, uint32_t q16);
void    audio_drain(AudioStreamId id);        /* 等待所有数据播放完毕 */
void    audio_close(AudioStreamId id);

/* 全局控制 */
void audio_set_master_volume(uint32_t q16);
int  audio_get_stats(AudioStats *stats);
```

这些 API 不是原版 AHI 调用名。实现后可在上面包一层真正的 AHI 兼容接口。

---

## 9. AHI 兼容层（后续）

- AHI：优先作为跨硬件抽象层，必要时实现公开版本的 API。
- Paula/audio.device 原生功能：仅在需要经典 Amiga 硬件/旧程序兼容时实现。
- 多声音设备路由、采样率转换、录音、MIDI：后续版本。
- USB Audio、现代 HDA：等 USB/PCI 设备框架成熟后再做。
- TLS/加密：不在音频栈范围内。

---

## 10. 验收

- 正弦波测试音稳定输出，无明显破音（440Hz，持续 5 秒，无爆裂声）。
- 两个 PCM 流同时播放且可独立调节音量。
- 低于/高于设备采样率的流可以经重采样正常播放（如 22050Hz 流在 48000Hz 设备上）。
- 暂停、恢复、关闭与设备移除不会死锁或访问已释放缓冲区。
- 记录 underrun、DMA 错误等统计信息。
- 10 个 stream 同时播放，混音器不崩溃，无明显音频失真。
- 设备写入阻塞时，audio_write 应返回已写入的帧数，不丢数据。

---

## 11. 参考

- [AmigaOS AHI Device](https://wiki.amigaos.net/wiki/AHI_Device)
- [AmigaOS Workbench AHI Preferences](https://wiki.amigaos.net/wiki/Workbench/Prefs/AHI)
- [AROS Contributor Guide — audio driver status](https://developers.aros.org/documentation/contribute.html)
