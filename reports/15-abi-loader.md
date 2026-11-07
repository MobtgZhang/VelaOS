# 15｜二进制格式、模块装载、ABI 与跨架构兼容

## 现有系统机制

- **经典 AmigaOS/m68k**：具有特定的 m68k 机器码、调用约定、函数向量与 Amiga HUNK 可执行格式；部分关键 DOS 数据结构继承 BCPL 指针（如 `BPTR`）语义。
- **AmigaOS 4.x/PowerPC**：有 PPC 原生程序与 ELF 相关机制，4.x 的现代库接口使用 `GetInterface()` 等调用方式，并与经典 3.x 的某些 ABI 细节不同。
- **AROS**：目标是跨架构的 AmigaOS 3.1 **API 级兼容**。其 `amiga-m68k` 移植旨在做到经典 AmigaOS 的**二进制兼容**；在其他 CPU 上，传统 m68k 二进制通常需要仿真/翻译，或重编译。AROS DOS 文档涉及 ELF/HUNK 装载。

**必须记住**：同名 API ≠ 同一二进制 ABI。跨 CPU 时还涉及指令集、端序、指针大小、对齐、结构体布局和系统库调用约定。

## 建议三层 ABI 设计

```text
         app (.elf or .hunk)
                 │
      Executable format parser
                 │
          Relocator / Symbol lookup
                 │
    ABI adaptation (native / m68k-compat)
                 │
   Exec / DOS / Intuition library interfaces
```

## 新系统的 native 装载器

建议首版仅支持一类明确定义的 ELF：

1. 核验 ELF magic、ELF class、机器架构、endianness、入口点。
2. 解析 `PT_LOAD`，映射页并初始化 BSS。
3. 实施 W^X（可写与可执行不应同时开放，除非有受控需求）。
4. 处理受支持的重定位/符号导入，不支持的类型明确报错。
5. 生成应用栈、`argv`/启动参数和程序初始上下文。
6. 安全失败路径：部分映射要回滚，禁止路径越界和整数溢出。
7. 进程退出后释放文件、端口、映射及其它资源。

### ELF loader 内部状态机

```text
UNOPENED → HEADER_OK → SEGMENTS_MAPPED → RELOCATED
                                            │
                                       RUNNABLE
                                            │
                                         EXITED
```

**首版可静态链接**，减少动态库重定位复杂度；Exec Library ABI 另行在 `03-exec-libraries.md` 实现。

## 经典兼容层的难点

### 1. CPU 架构

- x86-64 无法直接执行 m68k 指令。
- “加载 HUNK 文件并跳到入口地址”只在相应 CPU/仿真环境中才可能运行。
- 可选择 m68k 模拟器作为隔离的兼容子系统，或专注源码兼容。

### 2. 大端与小端

- m68k 经典系统主要是大端，PC x86 是小端。
- 结构体中的指针与嵌套对象不能通过简单 `byteswap` 全部自动转换。
- 文件格式和网络协议需要固定序列化格式，与内部结构体分开。

### 3. 32 位与 64 位

- AROS 的 `IPTR` 用于保存指针大小的值。
- 遗留代码将指针强制转为 32 位 `ULONG` 会在 64 位平台截断。
- 兼容头文件要明确 `IPTR`/`APTR`、`BPTR` 与 `LONG` 的真实宽度。

### 4. BPTR

经典 m68k DOS 中 `BPTR` 的表示与普通指针不同；AROS 在非二进制兼容移植上可能采用不同内部表示。使用 `BADDR()`、`MKBADDR()` 等对应宏转换，**不允许直接凭猜测位移**。

### 5. Library Vector / Interface

- AmigaOS 3.x 的 LVO 偏移是 ABI 组成部分。
- AmigaOS 4.x 的 Interface 是另一种调用机制。
- 不要把两种 ABI 混用成同一个函数指针表。

## 可执行目标决策表

| 目标 | 推荐路径 |
| --- | --- |
| 快速跑通你自己的 OS 应用 | x86-64 ELF + 自定义 native ABI |
| 源码兼容经典 Amiga 程序 | 重建 Exec/DOS/Graphics/Intuition API、重编译 |
| 执行现有经典 m68k 二进制 | m68k 平台/模拟器 + HUNK/系统 ABI |
| 运行已有 AROS x86_64 应用 | 需要匹配该 AROS ABI/运行时，不能凭 API 相似假设成功 |
| 运行 AmigaOS 4 PPC 二进制 | 需要 PPC 执行环境、相应 ABI 与库语义 |

## 安全检查与验收

- 过短/损坏/畸形 HUNK/ELF 文件被拒绝，且内存没有泄漏。
- 目标机架构与可执行文件不匹配时明确报告错误。
- ELF section/segment size 与 offset 的加法做溢出检查。
- 用户传入的 `argv`/环境/路径具备长度限制。
- 相同版本的 `hello.library` 与 `Hello App` 通过动态链接成功运行。
- 函数指针、`TagItem`、`BPTR` 在 32/64 位测试中符合目标 ABI。

## 参考

- [AROS Exec Library — IPTR/BPTR](https://developers.aros.org/documentation/app-dev/exec-library.html)
- [AROS DOS Autodocs — LoadSeg/ELF/HUNK](https://developers.aros.org/documentation/autodocs/dos.html)
- [AROS Platform Ports](https://www.aros.org/introduction/ports.html)
- [AmigaOS Exec Libraries](https://wiki.amigaos.net/wiki/Exec_Libraries)
- [AmigaOS Programming Environment](https://wiki.amigaos.net/wiki/Programming_in_the_Amiga_Environment)
