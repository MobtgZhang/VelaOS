# AmigaOS 与 AROS 系统目录结构总结

> 本文概述 AmigaOS（重点为经典 AmigaOS 3.x，补充 AmigaOS 4.x）与 AROS 的常见系统目录、逻辑路径、启动脚本和配置位置。目录因系统版本、硬件架构及发行版而异；下列结构为**示意**，不是每套安装都会包含全部文件夹。

## 1. 核心概念

AmigaOS 和 AROS 都采用按功能组织的系统目录，并大量使用带冒号的路径：

- `SYS:`：通常表示系统卷（系统分区）。
- `C:`、`LIBS:`、`DEVS:`、`S:` 等：常见逻辑路径（Assign），通常指向 `SYS:` 下的对应目录，也可以重新指向其他位置。
- 与 Linux 的 `/bin`、`/etc`、`/usr` 和 Windows 的 `C:\Windows` 不同，Amiga 系统的软件往往通过这些逻辑名称查找命令、库和配置。

**注意：** `SYS:C/` 表示系统卷上的实际目录，而 `C:` 是逻辑路径；两者通常相关，但不必始终相同。

## 2. AmigaOS 典型目录

以经典 AmigaOS 3.x / Workbench 为主要参考：

```text
SYS:
├── C/                 Shell 命令
├── Classes/           BOOPSI 类、DataTypes 等图形和数据类型组件
├── Devs/              设备驱动及配置
│   ├── DOSDrivers/    DOS 设备挂载描述
│   ├── DataTypes/     数据类型描述
│   ├── Keymaps/       键盘映射
│   ├── Monitors/      显示模式/显示器配置
│   └── Printers/      打印机配置
├── Fonts/             字体
├── L/                 文件系统和 Handler 等
├── Libs/              共享库
├── Locale/            本地化资源
├── Prefs/             系统偏好设置程序及持久化配置
│   └── Env-Archive/   常见的持久化环境配置存放位置
├── S/                 启动脚本
│   ├── Startup-Sequence
│   └── User-Startup
├── Storage/           未启用/备用的驱动与配置
├── System/            系统管理工具
├── Tools/             附加工具
├── Utilities/         实用程序
└── WBStartup/         Workbench 桌面自动启动程序
```

### AmigaOS 4.x 的补充

AmigaOS 4.x 继承大量经典目录约定，但还可能使用：

- `SYS:Kickstart/`：从磁盘加载的启动/内核相关模块。
- `SYS:SObjs/`：ELF 共享对象（`.so`）。
- 其他目录可能随版本及安装内容变化。

经典 AmigaOS 3.x 的 Kickstart 核心通常来自 ROM；不能据此假定所有版本都具有相同的磁盘目录。

## 3. AROS 典型目录

AROS 延续了 AmigaOS 的目录约定，同时为现代驱动、图形界面和开发工具提供扩展。以下是常见发行版可能出现的**综合示意**：

```text
SYS:
├── boot/               某些原生 PC 安装的引导文件
├── C/                  Shell 命令
├── Classes/            图形类、DataTypes，以及部分 Zune 组件
├── Devs/               设备驱动与配置
│   ├── AHI/            音频相关驱动/配置（视发行版）
│   ├── DOSDrivers/
│   ├── Drivers/        部分 HIDD 等驱动
│   ├── Keymaps/
│   └── Monitors/
├── Fonts/              字体
├── L/                  文件系统和 Handler
├── Libs/               共享库
├── Locale/             本地化资源
├── Prefs/              系统设置
│   └── Env-Archive/    持久化配置
├── S/                  启动脚本
│   ├── Startup-Sequence
│   └── User-Startup
├── Storage/            备用驱动及配置
├── System/             系统工具
├── Tools/              附加工具
├── Utilities/          实用程序
├── WBStartup/          桌面自动启动程序
├── Development/        SDK 头文件和开发库（部分发行版）
├── Extras/             附加或第三方软件（部分发行版）
└── Demos/              示例程序（部分发行版）
```

AROS 的一些特点：

- **HIDD**：硬件无关设备驱动抽象；部分相关驱动可在 `Devs/Drivers/` 中找到。
- **Zune**：AROS 的 MUI 风格图形界面实现；部分组件可位于 `Classes/Zune/` 等位置。
- **Development**：部分系统或 SDK 包含 `include/`、`lib/` 等开发资源。
- **Wanderer**：AROS 常见的桌面环境，对应经典 AmigaOS 的 Workbench 桌面角色。
- `boot/` 不属于所有 AROS 安装的统一必备目录。

## 4. 主要目录对照

| 目录/逻辑路径 | 主要作用 | 两套系统的关系 |
| --- | --- | --- |
| `C:` / `SYS:C/` | 命令行程序 | 基本一致 |
| `LIBS:` / `SYS:Libs/` | 动态共享库 | 基本一致，具体实现有差异 |
| `DEVS:` / `SYS:Devs/` | 设备驱动及设备配置 | 基本一致，AROS 含其自身驱动体系 |
| `L:` / `SYS:L/` | 文件系统、Handler | 基本一致 |
| `SYS:Classes/` | 图形对象、DataTypes | AROS 还常使用 Zune |
| `S:` / `SYS:S/` | 系统启动脚本 | 基本一致 |
| `SYS:Prefs/` | 偏好设置 | 基本一致 |
| `SYS:Storage/` | 暂未启用的驱动及配置 | 基本一致 |
| `SYS:WBStartup/` | 桌面自动启动 | Workbench / Wanderer 使用 |
| `SYS:Kickstart/` | 磁盘启动模块 | 主要见于 AmigaOS 4.x |
| `SYS:Development/` | SDK / 开发资源 | 在部分 AROS 发行版中常见 |

## 5. Assign：逻辑路径与实际目录

Assign 将逻辑名称映射到一个或多个实际位置，程序因此不必依赖固定的硬盘分区名。

典型映射示例：

```text
C:       → SYS:C/
LIBS:    → SYS:Libs/
DEVS:    → SYS:Devs/
S:       → SYS:S/
```

常见 Shell 命令：

```text
ASSIGN            ; 查看逻辑路径分配
DIR SYS:          ; 列出系统卷根目录
DIR LIBS:         ; 列出当前 LIBS: 指向的位置
DIR DEVS:
DIR S:
```

修改映射的示例：

```text
ASSIGN LIBS: Work:MyLibraries
```

这会改变当前 `LIBS:` 的指向；**不建议在实际运行的系统中随意替换关键系统 Assign**，因为可能导致程序无法找到系统库。

## 6. 启动脚本与环境配置

两套系统通常沿用以下启动约定：

1. 系统引导并初始化内核及基础组件（具体方式随平台不同）。
2. 执行 `S:Startup-Sequence`，完成系统初始化。
3. 启动流程可能调用 `S:User-Startup`，执行用户或第三方软件的自定义初始化。
4. 启动 Workbench 或 Wanderer 等图形桌面；按配置执行桌面自动启动程序。

`ENV:` 和 `ENVARC:` 是常见的环境配置逻辑位置：

| 路径 | 典型含义 |
| --- | --- |
| `ENV:` | 当前会话正在使用的配置 |
| `ENVARC:` | 保存到磁盘的持久配置 |

在经典 AmigaOS 和一些 AROS 配置中，典型映射为：

```text
ENV:     → RAM:Env/
ENVARC:  → SYS:Prefs/Env-Archive/
```

这些是常见约定而非所有版本的强制实现；尤其在 AmigaOS 4.x 中，`ENV:` 的实现可能不同。

## 7. 按功能速记

- **运行核心：** `Libs`、`Devs`、`L`、`Classes`
- **命令与启动：** `C`、`S`
- **设置与配置：** `Prefs`、`ENV:`、`ENVARC:`
- **桌面与工具：** `System`、`Tools`、`Utilities`、`WBStartup`
- **资源与扩展：** `Fonts`、`Locale`、`Storage`
- **发行版扩展：** `boot`、`Development`、`Extras`、`Demos` 等（按实际版本而定）

**一句话总结：** AmigaOS 与 AROS 都以功能目录组织系统文件，并通过 `Assign` 将逻辑路径与物理存储位置解耦；AROS 在兼容经典布局的基础上扩展了现代驱动、Zune 界面和开发环境。
