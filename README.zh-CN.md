# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` 是一个轻量的异步 Rust Minecraft 服务器状态查询库。它支持 Java
Edition status 请求、Bedrock Edition 的 RakNet unconnected ping 与 1.26.51+
版本使用的 NetherNet join 端点、Java SRV 发现、局域网发现，以及常见服务器地址
输入的规范化。

公开 API 分为三层：

1. **一行查询** — `mc_motd::query(address)` 并发探测所有已编译的版本，返回
   第一个成功响应。
2. **可复用客户端** — `Client` 在多次查询间共享 DNS 处理、NetherNet 连接池与
   有界的传输类型缓存。
3. **协议编解码** — `mc_motd::protocol` 提供纯 encode/decode 能力（无 socket、
   无缓存），包含零拷贝解析路径。

## 快速开始

```rust
use mc_motd::{Details, query};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let status = query("play.example.com").await?;

    println!("transport={:?} elapsed={:?}", status.transport(), status.elapsed);
    println!("{}", status.motd);
    println!("{:?}", status.players);
    match &status.details {
        Details::Java(details) => println!("{}", details.raw_json),
        Details::Bedrock(bedrock) => println!("{bedrock:?}"),
        // non_exhaustive：未来新增协议无需再次破坏性发布。
        _ => {}
    }

    Ok(())
}
```

## 查询路径

| 调用                          | 行为                                                       |
| ----------------------------- | ---------------------------------------------------------- |
| `mc_motd::query(address)`     | 并发探测所有已编译版本；第一个成功响应直接返回。           |
| `mc_motd::java::query`        | 仅 Java：先查 SRV 记录，再探测直连端口。                   |
| `mc_motd::bedrock::query`     | 仅 Bedrock：RakNet 与 NetherNet 竞速，更快者胜出。         |
| `bedrock::raknet::query`      | 强制使用传统 RakNet UDP ping。                             |
| `bedrock::nethernet::query`   | 强制使用 NetherNet join 端点（HTTPS `GET /v1/join`）。     |
| `lan::discover(options)`      | 本地网络广播；绝不访问公网服务器。                         |

强制传输的查询直接返回该传输的错误；自动路径把每个探测的失败聚合为结构化的
`Error::NoCompatible*`，并携带 `Attempt { transport, target, error }`，调用方
仍可遍历原始错误链。

## Client

```rust
use std::time::Duration;
use mc_motd::Client;

let client = Client::builder()
    .timeout(Duration::from_secs(3))
    .cache_capacity(1024)
    .allow_insecure_http(false)
    .build();

let status = client.query("play.example.com").await?;
let bedrock = client.bedrock("be.example.com").await?;
let java = client.java("mc.example.com").await?;
```

`Client` 克隆成本极低，多个异步任务可以共享同一实例。单次查询的临时配置使用
`Client::query_with(address, QueryOptions)`——链式选项**叠加在**客户端默认值
之上，因此只为修改 timeout 而创建的临时选项不会重置 builder 中的端口与策略：

```rust
use std::time::Duration;
use mc_motd::{Client, Edition, QueryOptions};

let client = Client::default();
let status = client
    .query_with(
        "play.example.com",
        QueryOptions::default()
            .timeout(Duration::from_secs(2))
            .edition(Edition::Bedrock),
    )
    .await?;
```

## 统一状态模型

所有成功查询返回统一的 `Status`：

- `address` — 本次查询使用的规范化服务器地址。
- `motd` — 归一化的第一行：Java 描述文本、RakNet 第一行 MOTD 或 NetherNet
  服务器名。
- `players` / `version` — 归一化的玩家数量与版本信息。
- `elapsed` — 整个查询的端到端耗时（含 DNS 与全部探测）。对 NetherNet 而言是
  HTTPS 信令耗时，**不是**游戏连接 RTT——游戏本体需要 WebRTC 握手。
- `details` — 协议原始数据：Java 原始 JSON（含 SRV 目标）、RakNet pong 字段、
  NetherNet join 响应（`nonce`、`onlineAuth`、`transportLayer`……）。
  `edition()` 与 `transport()` 从 `details` 推导，不存在两份可能漂移的状态。

## 查询行为

- 自动探测并发轮询 Java 与 Bedrock，且两侧立即启动：Java 在自己的探测内处理
  SRV（预算封顶 1 秒）与 DNS 解析，Bedrock 竞速以 NetherNet（独立解析栈）先行、
  RakNet 惰性解析并行展开。任何串行的 SRV/DNS 阶段都不会延迟已经就绪的传输。
  第一个成功响应直接返回，平手时 Java 优先；存在 SRV 记录时对 Java 侧保持权威。
- Bedrock 内部 RakNet 与 NetherNet 探测并发竞速：更快的传输直接胜出，平手时
  NetherNet 优先。命中缓存的传输先行探测，若它与回退传输都失败，聚合错误会
  保留两次探测的失败。
- 成功应答的传输类型按 `host:port` 缓存在所属 `Client` 内（10 分钟 TTL、有界
  容量），后续查询跳过失败探测。`client.cached_transport()` 与
  `client.clear_transport_cache()` 提供读写入口。
- `timeout` 是端到端 deadline：SRV、DNS 与全部探测共享同一截止时间，而不是
  每个阶段各自重新计时。
- Java 直连探测与所有 RakNet 传输路径都会在共享 deadline 内依次尝试所有
  解析出的地址（IPv4 优先）。
- NetherNet HTTPS 跑在游戏端口上；只有在传输层失败且安全策略允许时
  （`allow_insecure_http`，默认开启）才回退一次明文 `http://`。收到任何
  HTTP 状态码都不会触发回退。
- SRV 记录按优先级排序，同优先级内使用 RFC 2782 加权随机选择；零权重记录的
  区间为空，只有在没有其他记录时才会被排序。显式指定端口时完全跳过 SRV。
- 状态包中 JSON 无法解析——或不是 JSON 对象——属于协议错误
  （`Error::InvalidPacket`），不会产生空白状态；缺失的可选字段保持宽容。
  JSON 每次查询只解析一次。
- SRV 记录指向 `.` 或端口 0 表示服务明确不可用，会作为 attempt 错误暴露，
  而不是静默回退直连。
- `allow_insecure_http` 默认为**禁用**：明文 HTTP 的 NetherNet 应答未经身份
  验证，需要明文兼容的调用方必须在 builder 或单次查询中显式开启。
  `Transport::NetherNetHttp` 表示不安全的明文应答，`NetherNetHttps` 表示
  经过证书验证的应答。
- LAN 发现按 socket 地址去重（同一主机上的多个服务器都会上报）、限制结果
  数量（`max_results`，默认 32），并将 pong 与 ping 时间戳关联以忽略过期
  回复。
- 整个 crate `forbid unsafe_code`，CI 增加 `cargo audit` 依赖审计与 Windows
  平台任务。

## 协议层

```text
mc_motd::protocol::java::encode_status_request(...)
mc_motd::protocol::java::parse_status_response(...)
mc_motd::protocol::bedrock::raknet::encode_ping()
mc_motd::protocol::bedrock::raknet::decode_pong(...)
mc_motd::protocol::bedrock::nethernet::parse_join(...)
```

零拷贝路径：

- `bedrock::raknet::query_addr_into(address, buffer, timeout)` 把 pong 读入
  调用方缓冲区并返回借用的 `RakNetDetailsRef`；跨多次轮询复用一个缓冲区即可
  保持稳态零显式分配（socket 创建等运行时内部行为除外）。connect、send、
  receive 共享同一条 deadline。
- `protocol::bedrock::raknet::decode_pong` 与 `decode_motd_text` 解析为借用的
  `Cow` 字段，全程无堆分配：含分号的首行 MOTD 是输入中的一段连续字节区间，
  直接整体借用。
- `protocol::bedrock::nethernet::parse_join_ref` 通过 `#[serde(borrow)]` 把
  join JSON 反序列化为借用的 `Cow` 字段。

已实测可用的 NetherNet 服务器（端口均为游戏端口 19132）：`play.enchanted.gg`、
`nethernet.playhive.com` 与 `mco.lbsg.net`。

## Feature 开关

| Feature     | 默认 | 启用内容                                                          |
| ----------- | ---- | ----------------------------------------------------------------- |
| `java`      | 是   | Java 探测、SRV 查询（`hickory-resolver`）、JSON 状态解析。        |
| `bedrock`   | 是   | RakNet 探测与统一的 Bedrock 状态模型。                            |
| `nethernet` | 是   | NetherNet HTTPS 查询（`reqwest` + rustls，隐含 `bedrock`）。      |
| `lan`       | 否   | 局域网发现广播（`socket2`，隐含 `bedrock`）。                     |

- 纯 Bedrock 构建：`default-features = false, features = ["nethernet"]` — 没有
  Java 代码、没有 SRV、没有 DNS 解析器依赖。
- 纯 RakNet 构建：`default-features = false, features = ["bedrock"]` — 完全没有
  HTTP 或 TLS 依赖。

## 示例

```text
cargo run --example query -- play.example.com
cargo run --example bedrock_only -- nethernet.playhive.com:19132
cargo run --example lan_discovery --features lan            # 在 UDP 7551 上广播
cargo run --example lan_discovery --features lan -- 19132   # 原版基岩局域网端口
```

## 测试

测试套件使用解析 fixture 和本地 loopback mock server，不依赖公网 Minecraft
服务器。真机测试标记为 `#[ignore]`：

```text
cargo test
cargo test --features lan -- --ignored   # 含局域网广播
```

质量门：

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## 当前限制

- Java 状态对归一化字段采用宽松解析；完整组件树渲染请使用 `JavaDetails` 中的
  原始 JSON。
- SRV 查询只用于 Java Edition，遵循 Minecraft 的 `_minecraft._tcp` 约定。
- 自动探测按上述优先级返回第一个成功响应；需要特定版本或传输时应显式调用
  `java::query`、`bedrock::raknet::query` 或 `bedrock::nethernet::query`。
- NetherNet 的 elapsed 反映 HTTP 信令耗时，不代表 WebRTC 游戏链路。
- 局域网发现每次调用发送一次 IPv4 广播，不订阅周期性的局域网广播。
