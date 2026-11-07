# 12｜音频设备、AHI、PCM 混音与实时调度

## 现有系统机制

经典 Amiga 机器的音频有 Paula 等硬件路径及 `audio.device`；更通用的高层音频抽象是 **AHI**，目标包括硬件无关、多个应用共享音频设备以及录制/播放能力。不同 AmigaOS、AROS 发行版的实际声卡驱动和 AHI 支持不应一概而论。

## 推荐音频栈

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
      (VirtIO / HDA / Paula)
                │
         DMA ring buffer
                │
          Audio Hardware
```

## 最小能力

**第一步：** 即使没有声卡，也实现写 PCM 到环形缓冲区的 null/sink 后端，保证端到端 API 可测试。

**第二步：** 使用 QEMU 可仿真的声卡后端，支持 16-bit signed PCM、立体声、48kHz 输出。

**第三步：** 两个应用同时提交音频数据，能在混音器中合成。

## 音频流结构（示意）

```c
typedef struct {
    uint32_t sample_rate;
    uint16_t channels;
    uint16_t format;    // PCM_S16, PCM_F32 ...
    RingBuffer pcm;
    atomic_uint underruns;
    uint32_t volume_q16;
} AudioStream;
```

## 混音线程注意事项

- 尽可能避免在音频实时路径分配内存。
- 避免等待文件锁、窗口锁和其他高延迟系统服务。
- 流生命周期须防止“移除 stream 时 DMA 回调仍使用它”。
- 设备环形缓冲区需要明确写指针、读指针和缓存一致性。
- 提供欠载（underrun）计数器和可观察日志。
- 建议音频 Task 使用适当优先级，但不能永远饿死其他 Task。

## 推荐 API（设计方案）

```c
AudioStreamId audio_open(sample_rate, channels, sample_format);
size_t audio_write(AudioStreamId, const void *pcm, size_t frames);
void audio_set_volume(AudioStreamId, uint32_t q16);
void audio_drain(AudioStreamId);
void audio_close(AudioStreamId);
```

这些 API 不是原版 AHI 调用名。实现后可在上面包一层真正的 AHI 兼容接口。

## 逐步兼容

- Paula/audio.device 原生功能：仅在需要经典 Amiga 硬件/旧程序兼容时实现。
- AHI：优先作为跨硬件抽象层，必要时实现公开版本的 API。
- 多声音设备路由、采样率转换、录音、MIDI：后续版本。
- USB Audio、现代 HDA：等 USB/PCI 设备框架成熟后再做。

## 验收

- 正弦波测试音稳定输出，无明显破音。
- 两个 PCM 流同时播放且可独立调节音量。
- 低于/高于设备采样率的流可以经重采样正常播放。
- 暂停、恢复、关闭与设备移除不会死锁或访问已释放缓冲区。
- 记录 underrun、DMA 错误等统计信息。

## 参考

- [AmigaOS AHI Device](https://wiki.amigaos.net/wiki/AHI_Device)
- [AmigaOS Workbench AHI Preferences](https://wiki.amigaos.net/wiki/Workbench/Prefs/AHI)
- [AROS Contributor Guide — audio driver status](https://developers.aros.org/documentation/contribute.html)
