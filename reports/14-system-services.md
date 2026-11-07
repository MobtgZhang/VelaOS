# 14｜系统服务：Shell、Locale、DataTypes、Icon、Preferences、ARexx

本报告定义 VelaOS 系统服务层的完整设计，覆盖 Shell 与启动脚本、Locale、DataTypes、Icon 与 Workbench、Preferences、脚本自动化，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

AmigaOS 的“系统服务”不是单一服务管理器，而是多个 Library、DOS 组件与应用组合：

| 模块 | 作用 |
| --- | --- |
| `dos.library` | 文件与 Process 环境、Shell 基础 |
| `utility.library` | TagList、Hook、常用辅助函数 |
| `locale.library` | 语言/地区、catalog 字符串、日期格式 |
| `datatypes.library` | 把文件类型映射到可用对象类/解码器 |
| `icon.library` | `.info` 图标、ToolTypes、默认工具 |
| `workbench.library` | 图标拖放、AppIcon/AppWindow/AppMenuItem |
| `iffparse.library` | IFF FORM/chunk 格式解析 |
| `commodities.library` | 全局热键/输入过滤（参见 `11-input.md`） |
| `Prefs` | 设置编辑器与持久环境变量 |
| ARexx / `rexxsyslib.library` | 应用间脚本自动化（可后做） |

不同 AROS 发行版对上述库的覆盖与行为不必完全一致。

---

## 2. A. Shell 与启动脚本

### 最小 Shell

```text
Exec/DOS Process
   │
parse command line
   │
expand Assign / resolve C:
   │
load executable
   │
set stdin/stdout/stderr
   │
wait / collect return code
```

首批命令：`DIR`、`TYPE`、`COPY`、`DELETE`、`MAKEDIR`、`ASSIGN`、`CD`、`ECHO`、`VERSION`、`RUN`、`CLEAR`、`PWD`、`LS`、`CAT`、`WHOAMI`、`HOSTNAME`。再实现脚本与变量替换。

### Shell 设计要点

- 需要明确 `S:Startup-Sequence` 与 `S:User-Startup` 的调用顺序：先执行 Startup-Sequence（系统级），再执行 User-Startup（用户级）。
- `ENV:` 代表当前会话配置；`ENVARC:` 代表持久配置的常见逻辑位置。
- CLI Process 必须持有有效的当前目录、标准 I/O 和返回码。
- 任意重定向与 Pipe 实现之前，先保证文件句柄关闭与进程退出协议。
- 提示符格式：`hostname@user> current_directory`（如 `vela@vela> RAM:/`）。
- `CLEAR` 命令：输出 form feed + 换行符，将内容推离可见区域。
- `ECHO`：回显参数文本，支持 `\n` 转义。

### 内置命令列表

| 命令 | 功能 |
| --- | --- |
| `DIR` | 列出目录内容 |
| `TYPE` | 显示文件内容 |
| `COPY` | 复制文件 |
| `DELETE` | 删除文件 |
| `MAKEDIR` | 创建目录 |
| `ASSIGN` | 设置/显示卷映射 |
| `CD` | 改变/显示当前目录 |
| `ECHO` | 输出文本 |
| `VERSION` | 显示系统版本 |
| `RUN` | 启动程序（不等待） |
| `CLEAR` | 清屏 |
| `PWD` | 显示当前目录 |
| `LS` | 列出目录（DIR 同义） |
| `CAT` | 连接并显示文件 |
| `WHOAMI` | 显示当前用户 |
| `HOSTNAME` | 显示主机名 |
| `EXIT` | 退出 Shell |

---

## 3. B. Locale

加载默认语言、日期、数字、本地化字符串。推荐架构：

```text
built-in English string IDs
              │
language catalog loader
              │
GetCatalogStr(id) → localized string or English fallback
```

- CATALOG 文件与字体编码问题分开处理。
- 对于新系统可内部统一 UTF-8，兼容层再考虑经典 8-bit 字符集。
- 支持运行时切换语言，GUI 重新加载字符串不崩溃。
- 默认语言为英语（en-US），回退策略：找不到 catalog 时使用内置英文字符串。

---

## 4. C. DataTypes

DataTypes 提供文件类型识别和对象式解码器接口。最小实现：

1. 读取文件头（限制最大探测字节数，如 256 字节）。
2. 由 extension + magic bytes 判定类型。
3. 选择 Decoder。
4. 为文本/图像/音频返回统一的 View/Preview 对象。
5. 严格限制输入长度与嵌套，避免恶意文件破坏系统。

优先实现：`text/plain`、PNG/JPEG（使用成熟解码库）、IFF/ILBM（为 Amiga 兼容）。

**DataTypes 接口（示意）：**

```c
typedef struct {
    uint32_t type;       /* DTST_TEXT / DTST_IMAGE / DTST_SOUND / DTST_AMIGA */
    void    *decoder_private;
    uint64_t size;
} DataTypeObject;

int   datatype_identify(const char *path, uint32_t *out_type);
int   datatype_open(const char *path, DataTypeObject **out);
void  datatype_close(DataTypeObject *obj);
int   datatype_get_text(DataTypeObject *obj, char *buf, uint64_t max);
int   datatype_get_image(DataTypeObject *obj, Surface **surf);
```

安全规则：
- 图片解码时限制最大图像尺寸（如 4096×4096），防止内存耗尽。
- IFF 嵌套深度有上限（如 16 层），防止递归爆炸。
- 损坏的文件头不应导致越界访问。

---

## 5. D. Icon 与 Workbench 参数

`.info` 文件关联数据对象、图像、ToolTypes、DefaultTool。桌面双击项目时：

```text
read DiskObject
   ├─ TOOL    → 启动二进制
   ├─ DRAWER  → 打开目录
   ├─ PROJECT → 启动 DefaultTool 并传项目路径
   └─ DISK    → 打开卷
```

不要把 `.info` 当作普通 JSON；兼容解析应按实际 DiskObject/IFF/相关格式规范实现。

**DiskObject 概略：**

```c
typedef struct {
    uint32_t do_type;        /* DISKTOOL / DISKPROJECT / DISKDRAWER / DISKDEVICE */
    char     do_default_tool[128];
    char     do_tool_types[128][64];  /* 工具类型键值对 */
    uint32_t do_current_x, do_current_y; /* 图标位置 */
    uint32_t do_icon_x, do_icon_y;       /* 图标尺寸 */
    /* 图像数据（IFF 格式或位图） */
} DiskObject;
```

---

## 6. E. Preferences

推荐设置键空间：

```text
SYS:Prefs/                 设置编辑器/资源
ENV:SYS/Display            当前会话设置（示意）
ENVARC:SYS/Display         持久设置（示意）
```

采用“加载→验证→应用→可选持久保存”的事务顺序。更改显示分辨率、键盘布局时提供可回滚路径。

**Preferences 设置类别：**

| 类别 | 示例键 |
| --- | --- |
| 显示 | 分辨率、位深、刷新率 |
| 键盘 | 布局、重复延迟、重复速率 |
| 鼠标 | 速度、双击阈值、左右手模式 |
| 声音 | 音量、通道、输出设备 |
| 网络 | IP 地址、DNS 服务器、DHCP 开关 |
| 桌面 | 背景颜色、图标网格大小 |

`Save` 按钮将设置写入 `ENVARC:`（持久）；`Use` 按钮仅应用到当前会话（`ENV:`），不写磁盘。

---

## 7. F. 脚本自动化

ARexx 是经典应用自动化的重要接口，但首个 OS MVP 不必支持完整语言。可先实现消息端口命令路由，再添加 Rexx 解释器的兼容层。

**第一阶段：命令路由**

- 应用通过 Exec 消息端口注册命令处理器。
- 外部脚本发送消息到端口，应用处理并回复。
- 命令格式：`命令名 参数...`，返回结果码。

**第二阶段：Rexx 兼容**

- 集成经维护的 Rexx 解释器（如 Regina 或经典 ARexx 派生）。
- 提供 `rexxsyslib.library` 接口供应用注册可 Rexx 调用的函数。

---

## 8. 验收

- `ASSIGN C: SYS:C` 后能够从 Shell 执行命令。
- 改变 Locale 后 GUI 重新加载字符串不崩溃。
- 损坏的图片不会导致 DataTypes 越界访问。
- `.info` ToolTypes 对启动程序生效（如 `DefaultTool` 正确启动关联程序）。
- 使用 `Save` 重启后设置仍在，使用 `Use` 不强制写磁盘。
- 脚本启动失败时给出可定位的错误码。
- Shell 支持 `CLEAR`、`PWD`、`LS`、`CAT`、`WHOAMI`、`HOSTNAME` 等命令。
- `S:Startup-Sequence` 启动时不卡住，可在预定时间内完成。
- 桌面双击 `.info` 图标启动对应程序。

---

## 9. 参考

- [AmigaOS Icon Library](https://wiki.amigaos.net/wiki/Icon_Library)
- [AmigaOS Workbench Library](https://wiki.amigaos.net/wiki/Workbench_Library)
- [AmigaOS Datatypes Library](https://wiki.amigaos.net/wiki/Datatypes_Library)
- [AmigaOS Locale Library](https://wiki.amigaos.net/wiki/Locale_Library)
- [AROS DOS Library](https://developers.aros.org/documentation/app-dev/dos-library.html)
