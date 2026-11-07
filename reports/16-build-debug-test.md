# 16｜构建系统、仿真调试、单元测试与 CI

## 现有系统机制

AROS 的公开构建文档描述跨平台编译（GNU make / configure / GCC 等），其 MetaMake 体系允许按模块生成目标；模块类型包括 library、device、resource、handler、hidd、datatype 等。

AROS 官方调试指南推荐优先在 **hosted** 环境开发，借助宿主机 GDB 等工具；硬件层面问题则常通过串口日志和原生仿真定位。

**注意**：官方部分文档标有未完成/过时提示；旧示例使用的 `--target`、编译器版本、包名和构建脚本需与当前仓库核对，不能无验证地当作 2026 年必定成功的命令。

## 自己开发 OS：推荐仓库结构

```text
project/
├── CMakeLists.txt / Makefile
├── toolchain/
├── include/
├── src/
│   ├── kernel/
│   ├── arch/x86_64/
│   ├── drivers/
│   ├── dos/
│   ├── graphics/
│   └── desktop/
├── tests/
│   ├── unit/
│   ├── integration/
│   └── fuzz/
├── images/
└── docs/
```

### 构建目标

| 命令（建议自定义） | 输出 |
| --- | --- |
| `make kernel` | 内核 ELF |
| `make initrd` | 初始 RAM 文件系统 |
| `make image` | QEMU 引导镜像 |
| `make run` | QEMU 正常启动 |
| `make debug` | QEMU 停 CPU 并开放 GDB stub |
| `make test` | 单元测试 + 仿真集成测试 |
| `make ci` | 无人值守回归与日志归档 |

这组 `make` 目标是**给你新 OS 的建议**，不代表 AROS 自带相同 target。

## QEMU 原生调试方案

```text
build image
    │
QEMU (serial -> log, monitor, GDB stub)
    │
GDB + symbol file
    │
break at boot / context switch / device complete
    │
trace register/memory/IRQ
```

建议开发阶段开启 frame-pointer、调试符号和无优化/低优化编译；发布时保留分离的符号文件。中断堆栈异常时先检查保存/恢复的寄存器集合。

## 三层测试

### 单元测试：尽量在宿主运行

- intrusive linked list（插入、删除、迭代）
- ring buffer（空/满/环绕）
- priority runqueue
- mutex / reference count
- path parser / Assign 解析
- ELF/HUNK parser
- FAT/RAMFS 数据结构
- pixel clipping/blitting

### 集成测试：QEMU

- 启动 → Task → MsgPort → RAMFS → Shell
- 文件打开 → DOS Packet → Handler → FileHandle
- 鼠标输入 → input.device → Intuition → IDCMP
- 双窗口移动 → Layers 受损区域 → framebuffer
- IORequest 异步提交与取消 → 设备完成消息

### Fuzz/压力测试

- ELF 文件头、文件名、DataTypes 图片头、网络报文。
- 端口销毁和回复竞争、Signal 丢唤醒、Spinlock 死锁。
- 随机 I/O 读写、磁盘断电、设备热拔出。
- 多核版本用随机 CPU 亲和性和压力线程寻找数据竞争。

## 日志与诊断

建议采用结构化日志级别：`TRACE`、`DEBUG`、`INFO`、`WARN`、`ERROR`、`PANIC`。

```text
[t=123456789 cpu=0 task=17 WARN dev=blk0]
I/O timeout: request=301 opcode=READ lba=128 count=8
```

每个请求赋予 request_id；复杂跨 Task 的调用链靠 request_id 追踪，而不是只靠打印栈。

## CI 验收门槛

- 每次提交都编译通过、单元测试通过、QEMU 至少启动到 Shell。
- 新代码 `-Wall -Wextra`，关键模块启用静态分析。
- 重要解析器在宿主启用 ASan/UBSan 等检查。
- 每次二进制接口变动更新 ABI 文档与兼容测试。
- 每次驱动改动重跑启动、卸载、取消和热拔出测试。
- 所有失败用非零退出码标记，避免 CI 假绿。

## 参考

- [AROS Compiling](https://developers.aros.org/documentation/compiling.html)
- [AROS Build System](https://developers.aros.org/documentation/app-dev/buildsystem.html)
- [AROS Debugging](https://developers.aros.org/documentation/debugging.html)
- [AROS Module Builder](https://developers.aros.org/documentation/sys-dev/modules.html)
