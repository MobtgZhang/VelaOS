# 19｜接口实现清单：从 API 到测试用例

> 用于追踪你的 OS **实际完成**的 API。这里列出的是常见 Amiga 风格接口；不同 AmigaOS 版本、AROS 架构的声明/调用约定存在差异，请以目标 SDK 为准。

## 1. Exec / Library

| 接口 | 目标行为 | 验收用例 | 状态 |
| --- | --- | --- | --- |
| `AllocMem/FreeMem` | 按条件申请与释放内存 | 不足内存/不匹配释放 | ☐ |
| `AllocVec/FreeVec` | 分配与自动记录大小 | 重复释放防护 | ☐ |
| `FindTask` | 获取指定/当前 Task | 空值查询当前任务 | ☐ |
| `SetTaskPri` | 修改优先级 | 饥饿压力测试 | ☐ |
| `Wait/Signal` | 事件位通知 | 信号与 Wait 竞争 | ☐ |
| `CreateMsgPort/DeleteMsgPort` | 创建与销毁消息端口（实际名称随 SDK） | 端口销毁安全 | ☐ |
| `PutMsg/GetMsg/ReplyMsg` | 发送/接收/回复 | 10 万条消息无丢失 | ☐ |
| `OpenLibrary/CloseLibrary` | 库查找与引用计数 | 并发打开与版本拒绝 | ☐ |
| `OpenResource` | 返回低级资源句柄 | 不存在的资源返回错误 | ☐ |

## 2. Device / I/O

| 接口 | 目标行为 | 验收用例 | 状态 |
| --- | --- | --- | --- |
| `OpenDevice/CloseDevice` | 设备及 unit 生命周期 | 多 unit 并发 | ☐ |
| `DoIO` | 同步 I/O | 快速和非快速完成 | ☐ |
| `SendIO` | 异步 I/O | 完成通知与回复端口 | ☐ |
| `CheckIO/WaitIO` | 查询/等待完成 | 不重用在途请求 | ☐ |
| `AbortIO` | 请求取消 | 与完成竞争只结束一次 | ☐ |

## 3. AmigaDOS

| 接口 | 目标行为 | 验收用例 | 状态 |
| --- | --- | --- | --- |
| `Open/Close` | 文件句柄操作 | 不存在文件/多次 close | ☐ |
| `Read/Write/Seek` | 读写、移动位置 | EOF/部分读写 | ☐ |
| `Lock/UnLock/DupLock` | 文件锁 | 目录删除/锁释放 | ☐ |
| `Examine/ExNext` | 目录元数据/枚举 | 空目录、目录变动 | ☐ |
| `AssignLock/AssignPath` 等 | 逻辑路径分配 | 多 Assign / 延迟解析 | ☐ |
| `DoPkt/ReplyPkt` | Handler Packet 往返 | 超时/损坏 packet | ☐ |
| `LoadSeg/UnLoadSeg` | 装载可执行段 | 损坏 ELF/HUNK | ☐ |

## 4. 图形与 GUI

| 接口 | 目标行为 | 验收用例 | 状态 |
| --- | --- | --- | --- |
| `SetAPen/RectFill` | 填充矩形 | 边界裁剪 | ☐ |
| `Move/Draw` | 光栅绘制 | 线段像素回归 | ☐ |
| `Text/TextLength` | 文字绘制与测量 | 字体缺失回退 | ☐ |
| `BltBitMap` | 位图拷贝 | pitch/重叠测试 | ☐ |
| `OpenScreen/OpenWindow` | Screen/Window 管理 | 窗口堆叠/关闭 | ☐ |
| `IDCMP` | 事件向窗口端口投递 | 关闭前回复消息 | ☐ |
| `NewObject/DoMethod/SetAttrs` | BOOPSI 对象生命周期/方法 | 创建/销毁/通知 | ☐ |

## 5. 服务

| 接口 | 目标行为 | 验收用例 | 状态 |
| --- | --- | --- | --- |
| `OpenCatalog/GetCatalogStr` | 本地化与回退 | catalog 不存在 | ☐ |
| `NewDTObject/DisposeDTObject` | DataTypes 对象 | 无效文件头 | ☐ |
| `GetDiskObject/PutDiskObject` | `.info` 图标 | 损坏/旧版 icon | ☐ |
| `Timer.device: TR_ADDREQUEST` | 延迟后回复 | 取消与到期竞争 | ☐ |
| `AHI` 基础播放接口 | 多流音频 | 欠载恢复 | ☐ |
| BSD socket 基础接口 | UDP/TCP 收发 | 断线/超时 | ☐ |

## 6. 完成度标记规范

建议只使用以下标记，防止过度宣称：

- `☐` 未实现。
- `△` 有框架/部分功能，API 未通过完整测试。
- `✓` 已通过所列验收（**必须记录测试结果和构建版本**）。
- `N/A` 决定不兼容该接口，并说明原因。

创建一个 CI 自动导出的 `api-status.md` 表比手动宣称“100% Amiga 兼容”更可信。

## 7. 参考资料

- [AmigaOS Exec Libraries](https://wiki.amigaos.net/wiki/Exec_Libraries)
- [AmigaOS Exec Device I/O](https://wiki.amigaos.net/wiki/Exec_Device_I/O)
- [AROS Exec Autodocs](https://developers.aros.org/documentation/autodocs/exec.html)
- [AROS DOS Autodocs](https://developers.aros.org/documentation/autodocs/dos.html)
- [AmigaOS Intuition Library](https://wiki.amigaos.net/wiki/Intuition_Library)
