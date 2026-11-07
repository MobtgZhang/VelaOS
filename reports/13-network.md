# 13｜网卡驱动、TCP/IP 与 bsdsocket.library

本报告定义 VelaOS 网络栈的完整设计，覆盖 NIC 驱动、链路层、IPv4/UDP/TCP、BSD socket 兼容层，以及所有必须通过的验收场景。

---

## 1. 现有系统机制

Amiga 风格网络应用常使用 **BSD sockets 风格的 `bsdsocket.library` 接口**。在不同年代，AmigaOS 的 TCP/IP 栈来自不同产品与发行组件；AmigaOS 4.x 及 AROS 的网络实现与网卡驱动支持随版本变化。

**截至 2026 年的 AROS 具体状态**：AROSTCP 项目代码文档记载已有 IPv4、IPv6、TCP、UDP、ICMP、SANA-II 以及 BSD socket API，实现还涉及 IPv6 邻居发现、SLAAC、DHCPv6、路由与防火墙。这些是项目代码能力，不意味着所有镜像或网卡都经过相同测试。

**注意**：网络栈 API 可用、网卡驱动存在、DHCP 工作、IPv6 工作和 TLS 可用是五件不同的事，不能混为一谈。**下文的 IPv4→UDP→TCP→IPv6 路线是建议你从零开发时采用的先后顺序，不是 AROS 的功能缺失清单。**

VelaOS 首版以 IPv4 + UDP + virtio-net 为起点，TCP 和 bsdsocket.library 作为后续目标。

---

## 2. 推荐协议栈边界

```text
Applications (DNS / HTTP / FTP ...)
          │
   bsdsocket.library facade
          │
      Socket Layer
          │
     TCP / UDP / ICMP
          │
         IPv4
          │
        ARP
          │
      Ethernet
          │
     NIC Driver API
          │
    VirtIO-Net / e1000
```

IPv6、TLS、NAT、Wi-Fi 和防火墙都可放到后续迭代，不需要首日实现。

---

## 3. 最小实现路线

### N0：驱动

在 QEMU 中使用 virtio-net 或 e1000。实现：
- RX/TX 缓冲区（描述符环）
- 链路状态（up/down、速度、双工）
- 中断处理
- 队列管理
- MAC 地址读取

**接口：**

```c
typedef struct {
    int  (*init)(void);
    void (*shutdown)(void);
    int  (*send)(const void *packet, uint16_t len);
    int  (*recv)(void *buf, uint16_t *len);   /* 非阻塞，无包返回 0 */
    void (*set_multicast)(const uint8_t *mac, uint32_t count);
    uint8_t (*get_mac)[6];
    int  (*get_link_status)(void);            /* 0=down, 1=up */
    void (*get_stats)(NicStats *);
} NicDriver;

int nic_register(NicDriver *drv, const char *name);
```

### N1：链路层

- 以太帧收发（Ethernet II 封装）
- MAC 地址（源/目的、广播 FF:FF:FF:FF:FF:FF）
- ARP 缓存及其过期机制（默认 60 秒）
- ARP 请求/应答处理

**ARP 表项：**

```c
typedef struct {
    uint8_t  mac[6];
    uint32_t ip;
    uint64_t last_update_ns;
    bool     resolved;
} ArpEntry;
```

### N2：IPv4

- 解析/生成 IPv4 包（版本、IHL、TOS、总长度、标识、标志、片偏移、TTL、协议、校验和、源/目的 IP）
- 校验和计算与验证
- MTU 与分片处理策略（首版可不支持分片，大于 MTU 的包直接丢弃）
- 静态地址配置（硬编码或通过命令行设置）
- 路由表（最长前缀匹配）

### N3：UDP

- 实现 socket、bind、sendto、recvfrom
- 先完成 DHCP 客户端与 DNS resolver（基于 UDP）
- UDP 校验和（可选，IPv4 中可设为 0 表示不校验）
- 端口绑定与多路复用

### N4：TCP

- 连接三次握手（SYN, SYN-ACK, ACK）
- 可靠重传（超时重传、快速重传）
- 滑动窗口流量控制
- 拥塞控制（慢启动、拥塞避免、快速恢复）
- FIN/RST、超时与关闭状态机（ESTABLISHED, FIN_WAIT, TIME_WAIT, ...）

### N5：BSD sockets 兼容层

映射 Amiga/AROS 期望的 errno、socket descriptor、signal/interruption 语义与库打开关闭模型。

**示例内部接口（非真实 bsdsocket ABI）：**

```c
int  net_socket(int domain, int type, int protocol);
int  net_bind(int fd, SockAddr *address);
int  net_connect(int fd, SockAddr *address);
int  net_listen(int fd, int backlog);
int  net_accept(int fd, SockAddr *address);
ssize_t net_send(int fd, const void *data, size_t len);
ssize_t net_recv(int fd, void *data, size_t len);
int  net_close(int fd);
int  net_setsockopt(int fd, int level, int optname, const void *optval, uint32_t optlen);
```

---

## 4. 网络 Task 与内存所有权

- IRQ 仅回收/发布收包事件，解析协议在 worker 上下文完成。
- RX 数据包采用引用计数或明确所有权，避免重复释放。
- 超时/重传应使用单调时钟。
- Socket 的阻塞等待可映射到 Exec Signal/MsgPort 机制。
- 网卡断开时，等待中的连接需要错误传播和唤醒。
- 不可信网络包必须做长度与边界检查：IP 长度、TCP offset、DNS 压缩指针等。

---

## 5. 提升兼容性的注意点

- DOS 文件句柄与 BSD socket 描述符可能并不是同一套整数/对象，需要专门适配。
- `select()`/等待网络事件必须遵守 Exec Task 信号位策略。
- 处理非阻塞模式、`EINTR` 类中断及应用退出取消。
- IPv4/IPv6 能力需分别列出；没有实现的 API 应返回明确错误，而不是静默假成功。
- TLS 最好基于经维护的密码库实现，**不要自创加密协议**。

---

## 6. 验收

- QEMU 环境可 Ping 网关或主机（依据 ICMP 支持）。
- DHCP 获得地址、DNS 解析本地测试域名。
- UDP 回显 1 万个报文，不越界。
- TCP 回环发送 100MB 数据并比较校验和。
- 重传、连接断开、DNS 超时和网卡复位都能被正确报告。
- fuzzing 畸形 Ethernet/IP/TCP/DNS 包不导致内核崩溃。
- 网卡初始化失败时，网络栈应返回明确错误，不崩溃。
- 并发发送/接收不导致包丢失或内存损坏。

---

## 7. 参考

- [AROS Developer Portal](https://developers.aros.org/)
- [AROS Current Project News](https://aros.org/)
- [AROS Portability and Compatibility FAQ](https://www.aros.org/documentation/faq.html)
- [AROSTCP 技术文档（2026）](https://github.com/aros-development-team/AROS/wiki/AROSTCP)
- [AmigaOS Network Programming](https://wiki.amigaos.net/wiki/Developing_Network_Applications)
