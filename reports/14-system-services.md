# 14｜系统服务：Shell、Locale、DataTypes、Icon、Preferences、ARexx

## 现有系统机制

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

## A. Shell 与启动脚本

最小 Shell：

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

首批命令：`DIR`、`TYPE`、`COPY`、`DELETE`、`MAKEDIR`、`ASSIGN`、`CD`、`ECHO`、`VERSION`、`RUN`。再实现脚本与变量替换。

- 需要明确 `S:Startup-Sequence` 与 `S:User-Startup` 的调用顺序。
- `ENV:` 代表当前配置；`ENVARC:` 代表持久配置的常见逻辑位置。
- CLI Process 必须持有有效的当前目录、标准 I/O 和返回码。
- 任意重定向与 Pipe 实现之前，先保证文件句柄关闭与进程退出协议。

## B. Locale

加载默认语言、日期、数字、本地化字符串。推荐：

```text
built-in English string IDs
             │
language catalog loader
             │
GetCatalogStr(id) → localized string or English fallback
```

CATALOG 文件与字体编码问题分开处理；对于新系统可内部统一 UTF-8，兼容层再考虑经典 8-bit 字符集。

## C. DataTypes

DataTypes 提供文件类型识别和对象式解码器接口。最小实现：

1. 读取文件头（限制最大探测字节数）。
2. 由 extension + magic bytes 判定类型。
3. 选择 Decoder。
4. 为文本/图像/音频返回统一的 View/Preview 对象。
5. 严格限制输入长度与嵌套，避免恶意文件破坏系统。

优先实现：`text/plain`、PNG/JPEG（使用成熟解码库）、IFF/ILBM（为 Amiga 兼容）。

## D. Icon 与 Workbench 参数

`.info` 文件关联数据对象、图像、ToolTypes、DefaultTool。桌面双击项目时：

```text
read DiskObject
   ├─ TOOL    → 启动二进制
   ├─ DRAWER  → 打开目录
   ├─ PROJECT → 启动 DefaultTool 并传项目路径
   └─ DISK    → 打开卷
```

不要把 `.info` 当作普通 JSON；兼容解析应按实际 DiskObject/IFF/相关格式规范实现。

## E. Preferences

推荐设置键空间：

```text
SYS:Prefs/                 设置编辑器/资源
ENV:SYS/Display            当前会话设置（示意）
ENVARC:SYS/Display         持久设置（示意）
```

采用“加载→验证→应用→可选持久保存”的事务顺序。更改显示分辨率、键盘布局时提供可回滚路径。

## F. 脚本自动化

ARexx 是经典应用自动化的重要接口，但首个 OS MVP 不必支持完整语言。可先实现消息端口命令路由，再添加 Rexx 解释器的兼容层。

## 验收

- `ASSIGN C: SYS:C` 后能够从 Shell 执行命令。
- 改变 Locale 后 GUI 重新加载字符串不崩溃。
- 损坏的图片不会导致 DataTypes 越界访问。
- `.info` ToolTypes 对启动程序生效。
- 使用 `Save` 重启后设置仍在，使用 `Use` 不强制写磁盘。
- 脚本启动失败时给出可定位的错误码。

## 参考

- [AmigaOS Icon Library](https://wiki.amigaos.net/wiki/Icon_Library)
- [AmigaOS Workbench Library](https://wiki.amigaos.net/wiki/Workbench_Library)
- [AmigaOS Datatypes Library](https://wiki.amigaos.net/wiki/Datatypes_Library)
- [AmigaOS Locale Library](https://wiki.amigaos.net/wiki/Locale_Library)
- [AROS DOS Library](https://developers.aros.org/documentation/app-dev/dos-library.html)
