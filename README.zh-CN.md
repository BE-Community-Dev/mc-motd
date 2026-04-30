# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` 是一个轻量的异步 Rust Minecraft MOTD 查询库。它支持 Java
Edition status 请求、Bedrock Edition RakNet unconnected ping、Java SRV
发现，以及常见服务器地址输入的规范化。

这个 crate 刻意保持 API 小而直接：Java status 以原始 JSON 返回，方便应用层选择
自己的 JSON 模型；Bedrock MOTD 则解析为带可选数值字段的结构体。

## 推荐 API

- `query_minecraft_address(address, timeout)` 是最方便的入口，支持 `host`、
  `host:port` 和 `[ipv6]:port`。没有显式端口时，库会自动使用 Java 和 Bedrock
  的默认端口。
- `query_minecraft(host, port, timeout)` 查询一个显式端口，并跳过 SRV。
- `query_minecraft_with_options(host, options)` 可配置 Java/Bedrock 独立端口、
  timeout 和 SRV 行为。
- `query_java` 和 `query_bedrock` 是版本专用查询函数。
- `parse_java_status_response`、`parse_bedrock_pong` 和
  `parse_bedrock_motd_text` 可用于测试、代理和自定义传输层。

```rust
use std::time::Duration;
use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;

    match result.motd {
        MinecraftMotd::Java(status) => println!("{}", status.raw_json),
        MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
    }

    Ok(())
}
```

## 查询行为

- Java Edition 通过 TCP 发送 status handshake 和 request packet。
- Bedrock Edition 通过 UDP 发送 RakNet unconnected ping。
- 地址查询在没有显式端口且 host 是 DNS 名称时，会先尝试 Java SRV 记录。
- 自动探测会先试 Java，再试 Bedrock，并返回第一个成功响应。
- timeout 由调用方控制，并覆盖公开查询函数里的 DNS 查询和网络 I/O。

## 错误模型

所有可能失败的 API 都返回 `MotdError`。它区分输入无效、DNS 失败、网络 I/O、
timeout、响应包无效、UTF-8 无效，以及 Java/Bedrock 都无法查询成功的情况。

## 示例

```text
cargo run --example query -- play.example.com
cargo run --example query -- play.example.com:25565
```

## 测试

测试套件使用解析 fixture 和本地 loopback mock server，不依赖公网 Minecraft 服务器。

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## 当前限制

- Java status 以原始 JSON 暴露，而不是强类型 schema。
- SRV 查询只用于 Java Edition，遵循 Minecraft 的 `_minecraft._tcp` 约定。
- 自动探测返回第一个成功的版本响应；如果应用需要同时查询两个版本，应显式调用
  `query_java` 和 `query_bedrock`。
