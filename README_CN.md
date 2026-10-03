# harbor

[![crates.io](https://img.shields.io/crates/v/harbor?style=flat-square&logo=rust)](https://crates.io/crates/harbor)
[![docs.rs](https://img.shields.io/docsrs/harbor?style=flat-square&logo=docs.rs)](https://docs.rs/harbor)
[![MSRV](https://img.shields.io/badge/MSRV-1.75-blue?style=flat-square)](https://blog.rust-lang.org/2023/12/28/Rust-1.75.0.html)
[![licence](https://img.shields.io/crates/l/harbor?style=flat-square)](#协议)

**Cargo 工作区的发布编排工具。** 算出该发什么、按什么顺序发，发布前先做检查，然后驱动 registry。

[English](README.md)

## 特性

- 🧮 **自动推导顺序** — 依赖图的拓扑排序，无需手工维护
- 🛑 **发布前拦截** — 依赖成环、`publish = false` 冲突、`members` 写错，都在上传前失败
- 🏷️ **Tag 校验** — tag 必须与 manifest 版本一致，registry token 必须存在且非空
- 🔁 **可续跑** — 已在 registry 上的版本视为完成，重跑即可接着做完一次中断的发布
- ⏱️ **只重试该重试的** — 索引没跟上就等，token 不对就停
- 🧪 **可测试** — 所有决策走 `CommandRunner`，不需要 registry
- 📚 **库优先** — CLI 只是公开 API 上的一层薄壳

## 安装

```console
$ cargo install harbor --locked        # 提供 `cargo harbor` 子命令
```

## 命令

```console
$ cargo harbor plan
5 crate(s) to publish, in this order:
   1. acme-core 0.4.0
   2. acme-macros 0.4.0
   3. acme-net 0.4.0
   4. acme-cli-lib 0.4.0
   5. acme-app 0.4.0

1 not published (publish = false):
  - acme-cli 0.4.0

$ cargo harbor check v0.4.0           # 校验 tag 与 manifest 版本，并检查 token 是否存在
$ cargo harbor check v0.4.0 --plan    # 顺带确认发布计划算得出来
$ cargo harbor publish                # 按上面的顺序上传
$ cargo harbor publish --dry-run      # 只打印将要发生什么
```

那个顺序完全来自依赖图：`acme-core` 和 `acme-macros` 不依赖任何东西所以排最前（按名字排序，
保证输出稳定），`acme-app` 等所有。

**CI 里建议加上 `--plan`**——算计划不花钱、不联网，它能在任何上传之前发现依赖成环或
`members` 写错。

### 选项

全局：

| 选项 | 含义 |
| --- | --- |
| `--root <dir>` | 工作区根目录（默认 `.`） |
| `--order <crate>` | 只发布这些 crate、按这个顺序（可重复，或逗号分隔） |
| `--prefix <p>` | 只把名字以 `p` 开头的 crate 作为候选（默认：不过滤） |
| `--no-locked` | 允许发布改动 lockfile（默认：拒绝） |
| `--wait <s>` | 索引未跟上时的重试间隔秒数（默认 10） |
| `--attempts <n>` | 索引未跟上时的重试次数（默认 60） |

子命令自有：`check --plan`、`publish --dry-run`。

`--order` 用于推导会出错的唯一情况：某个 crate 该发布、但 manifest 里是 `publish = false`。
它只负责选择与排序，不覆盖依赖图——把 crate 排在其依赖之前会被拒绝。写的是 **package 名**，
所以是 `harbor` 而不是 `cargo-harbor`。

## 作为库使用

```rust
use harbor::{Release, ReleaseConfig};

let release = Release::new(".").with_config(ReleaseConfig::default());

for krate in &release.plan()?.order {
    println!("{} {}", krate.name, krate.version);
}

let report = release.publish()?;
println!("{} published", report.published.len());
```

`Publisher::with_runner` 接受一个 `CommandRunner`，这正是让整个发布流程可以脱离 registry
测试的关键。

## 在 CI 中使用

```yaml
- run: cargo install harbor --locked

- name: Publish
  run: |
    # tag 校验只在打 tag 时有意义；publish 在任何地方跑都安全，
    # 因为计划算不出来时它会在上传前就失败。
    if [ "$GITHUB_REF_TYPE" = "tag" ]; then cargo harbor check --plan "$GITHUB_REF_NAME"; fi
    cargo harbor publish
  env:
    # 通过环境变量传入，绝不作为命令行参数：命令行参数对本机所有进程可见。
    CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

本仓库自己的发布方式是怎样的，见 [`.github/workflows/`](.github/workflows/)。

## 不做什么

不做版本号 bump、不做 changelog、不打 git tag、不打包二进制。这些是每个项目自己的决定。

## 协议

MIT.
