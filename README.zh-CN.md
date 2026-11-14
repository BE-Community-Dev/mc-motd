# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` 是一个轻量的异步 Rust Minecraft MOTD 查询库。它支持 Java
Edition status 请求、Bedrock Edition 的 RakNet unconnected ping 与
1.26.51+ 版本使用的 NetherNet join 端点、Java SRV 发现、局域网发现，以及
常见服务器地址输入的规范化。版本之间可以隔离：纯 Bedrock 构建可以完全去掉
Java 支持及其 DNS 依赖。

这个 crate 刻意保持 API 小而直接：Java status 以原始 JSON 返回，方便应用层选择
自己的 JSON 模型；Bedrock MOTD 则解析为带可选数值字段的结构体，并为高频路径
提供零拷贝的借用式变体。

## 推荐 API

- `query_minecraft_address(address, timeout)` 是最方便的入口，支持 `host`、
  `host:port` 和 `[ipv6]:port`。Java 与 Bedrock 探测并发执行，第一个成功响应
  直接返回。
- `query_bedrock_address(address, timeout)` / `query_bedrock_auto(host, port,
  timeout)` 是 Bedrock 专用入口：绝不接触 Java，RakNet 与 NetherNet 并发竞速，
  更快的传输直接胜出（平手时 NetherNet 优先）。
- `query_minecraft(host, port, timeout)` 查询一个显式端口，跨两个版本并发探测
  并跳过 SRV。
- `query_minecraft_with_options(host, options)` 可配置 Java/Bedrock 独立端口、
  timeout、SRV 行为以及 `try_java` 隔离开关。
- `query_java`、`query_bedrock` 和 `query_nethernet` 是版本/传输专用查询函数。
- `query_bedrock_addr_into(address, buffer, timeout)` 是零分配的 RakNet 路径：
  把 pong 读入调用方的缓冲区并返回借用的 `BedrockMotdRef`。
- `discover_lan_servers(options)` 通过 UDP 广播发现局域网内的 Bedrock 服务器。
- `parse_java_status_response`、`parse_bedrock_pong`、
  `parse_bedrock_motd_text`、`parse_nethernet_join_json` 和
  `parse_nethernet_join_json_ref` 可用于测试、代理和自定义传输层。

```rust
use std::time::Duration;
use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;

    println!("transport={:?} latency={:?}", result.transport, result.latency);
    match result.motd {
        MinecraftMotd::Java(status) => println!("{}", status.raw_json),
        MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
        MinecraftMotd::BedrockNetherNet(motd) => println!("{} {}", motd.name, motd.version),
        // 枚举是 non_exhaustive 的，未来新增版本无需再次破坏性发布。
        _ => {}
    }

    Ok(())
}
```

## 查询行为

- Java Edition 通过 TCP 发送 status handshake 和 request packet。
- Bedrock Edition 通过 UDP 发送 RakNet unconnected ping、通过 HTTPS 请求
  NetherNet join 端点（见下文），或两者并用，取决于服务器实际支持哪种。
- 自动探测并发执行 Java 与 Bedrock 探测：host 只解析一次并共享，两边同时
  开跑，第一个成功响应直接返回（平手时 Java 优先；存在 SRV 记录时 SRV 对
  Java 侧保持权威）。
- timeout 由调用方控制，并覆盖公开查询函数里的 DNS 查询和网络 I/O。

## NetherNet（基岩版 1.26.51+）

新版基岩服务器通过 NetherNet join 端点公布状态（可以与 RakNet ping 共存）：
`GET https://{host}:{游戏端口}/v1/join` 返回包含 `name`、`protocol`、
`version`、`level`、`players`、`maxPlayers`、`gameType` 的 JSON；任何 `2xx`
状态码都表示服务器支持 NetherNet。参见
[NetherNet 接入指南](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/)。

Bedrock 自动探测的行为：

- 服务器传输类型未知时，RakNet 探测与 NetherNet 探测并发执行，**更快的传输
  直接胜出**（平手时 NetherNet 优先）。同时支持双协议的第三方服务器总是拿到
  最快的答案。
- 成功应答的传输类型会按 `host:port` 缓存（`cached_transport`、
  `clear_transport_cache`），后续查询跳过失败探测。缓存条目 10 分钟过期。
- HTTPS 尝试发生在游戏端口上；只有在传输层失败（连接、TLS 或超时）后才回退
  一次明文 `http://`，收到过任何 HTTP 状态码都不会触发回退。

专用函数：`query_nethernet(host, port, timeout)`、
`query_nethernet_with_options` 和 `query_nethernet_url`。NetherNet 查询复用
进程级共享的 `reqwest::Client`（连接池复用、5 秒连接超时、不跟随重定向），
默认限制响应体不超过 64 KiB，并校验玩家数量与协议字段。响应体直接从收到的
字节解析；`parse_nethernet_join_json_ref` 提供同样的解析但返回借用的 `Cow`
字段，轮询时零分配。

耗时语义：结果的 `latency` 是本次查询自身的往返耗时。对
`Transport::NetherNetHttp` 来说这是 HTTPS 信令耗时，**不是**游戏连接 RTT——
游戏本身的连接要经过 WebRTC 握手。

已实测可用的服务器（端口均为游戏端口 19132）：`play.enchanted.gg`、
`nethernet.playhive.com` 和 `mco.lbsg.net`。

## 隔离版本

以 Bedrock 为核心的应用可以完全去掉 Java：

- 使用 `default-features = false, features = ["nethernet"]` 构建只编译
  Bedrock 路径；`java` feature（默认开启）控制 Java 探测、SRV 查询和
  `hickory-resolver` 依赖。
- 每次调用可通过 `MotdQueryOptions::try_java = false` 跳过 Java 与 SRV，或直接
  使用 `query_bedrock_auto` / `query_bedrock_address` 专用入口。
- 零拷贝 RakNet 助手 `query_bedrock_addr_into` 在多次轮询间复用一个调用方
  缓冲区并返回借用的 `BedrockMotdRef`，让服务器列表轮询的稳态分配为零。

## 局域网发现

`discover_lan_servers` 在本地网络广播一个 RakNet unconnected ping 并收集
unconnected pong 回复。它与公网 MOTD 查询互相独立：局域网发现不会访问公网
NetherNet HTTP 端点，公网查询也绝不会在局域网内广播。

```rust
use std::time::Duration;
use mc_motd::{LanDiscoveryOptions, discover_lan_servers};

let servers = discover_lan_servers(LanDiscoveryOptions {
    listen_duration: Duration::from_secs(2),
    ..LanDiscoveryOptions::default()
}).await?;
```

默认广播端口是 7551。原版基岩局域网世界在 UDP 19132 上广播
（`VANILLA_LAN_PORT`）；需要发现原版主机时通过
`LanDiscoveryOptions::broadcast_port` 传入。回复按来源地址去重并排序，无法
解析的回复会被忽略。

## 错误模型

所有可能失败的 API 都返回 `MotdError`。它区分输入无效、DNS 失败、网络 I/O、
timeout、响应包无效、UTF-8 无效、HTTP 状态码异常（`HttpStatus`）、HTTP 传输
失败（`Http`），以及 Java/Bedrock 都无法查询成功的情况。

## 示例

```text
cargo run --example query -- play.example.com
cargo run --example query -- nethernet.playhive.com:19132
cargo run --example bedrock_only -- nethernet.playhive.com:19132
cargo run --example lan_discovery            # 在 UDP 7551 上广播
cargo run --example lan_discovery -- 19132   # 原版基岩局域网端口
```

## 测试

测试套件使用解析 fixture 和本地 loopback mock server，不依赖公网 Minecraft
服务器。真机测试标记为 `#[ignore]`，可用 `cargo test -- --ignored` 运行。

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## Feature 开关

- `nethernet`（默认启用）：通过 `reqwest`（rustls，webpki 根证书）和
  `serde`/`serde_json` 启用 Bedrock NetherNet 状态查询。
- `java`（默认启用）：启用 Java Edition 探测与 SRV 查询（`hickory-resolver`）。
- `default-features = false, features = ["nethernet"]` 是纯 Bedrock 构建：没有
  Java 代码、没有 SRV、没有 DNS 解析器依赖。
- `default-features = false` 是纯 RakNet 旧版行为，完全没有任何 HTTP 依赖。

## 当前限制

- Java status 以原始 JSON 暴露，而不是强类型 schema。
- SRV 查询只用于 Java Edition，遵循 Minecraft 的 `_minecraft._tcp` 约定。
- 自动探测按上述优先级返回第一个成功响应；如果需要特定版本或传输，应显式调用
  `query_java`、`query_bedrock` 或 `query_nethernet`。
- NetherNet 的 latency 反映 HTTP 信令耗时，不代表 WebRTC 游戏链路。
- 局域网发现每次调用发送一次 IPv4 广播，不订阅周期性的局域网广播。
