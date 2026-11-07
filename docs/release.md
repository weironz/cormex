# 构建候选与正式发布

构建、发布是两次独立操作；每次都能独立选择本地或 GitHub Actions。
本地构建 → Actions 发布、Actions 构建 → 本地发布都走同一个候选协议。
发布不等于部署，本流程不会 ssh、修改生产配置或执行生产 migration。

## 四个入口

| 命令 | 执行地点 | 结果 |
|---|---|---|
| `just build-local vX.Y.Z` | 本地 | 全检查、构建、烟测、打包，上传 draft candidate |
| `just build-github vX.Y.Z` | Actions | 全检查、六平台 CLI、两个镜像，聚合并上传 draft candidate |
| `just release-local vX.Y.Z BUNDLE_SHA256` | 本地 | 校验指定候选，推已有镜像，公开 Release |
| `just release-github vX.Y.Z BUNDLE_SHA256` | Actions | 经 production environment 发布同一候选 |

`build-github` / `release-github` 只是触发运行；触发成功不代表构建/发布成功。
发布 workflow 的手动表单使用 `tag` 和 `checksum` 两个输入，后者填构建时记录的
`BUNDLE_SHA256`。到对应 Actions run 查看结果。三个 workflow 都不接受 push / pull_request / tag 自动触发。
CI 的 `workflow_call` 只供 build 复用。历史的 `just release 0.1.14` 会失败并给出迁移提示。

## 准备源代码

1. 提交全部代码和版本变更，使工作树干净。构建入口要求 HEAD 正好等于输入 tag。
2. `Cargo.toml` workspace 版本与 tag（`vX.Y.Z`）一致，Cargo.lock 已提交且解析一致。
3. 把同一 tag 推到目标仓库。脚本会验证远端 tag，绝不会顺手创建一个指向默认分支的新 tag。
4. 对本次变更，先完成评审，再让构建处理该 tag。包含新脚本的流程无法直接在没有脚本的历史 tag 上运行。

示意命令（仅在确认准备发布的提交后执行）：

```bash
git tag -a vX.Y.Z -m vX.Y.Z
git push origin vX.Y.Z
just build-local vX.Y.Z
# 或：just build-github vX.Y.Z
```

## 本地工具与检查

- Python 3.11+（使用标准库 tomllib）、Git、just、Rust/rustup（版本由 rust-toolchain.toml 钉住）。
- Docker Engine + buildx，能够构建/运行 linux/amd64；非 amd64 主机需可用的仿真。
- `gh` 已对目标仓库登录。构建上传 draft 需要 contents write，但不需要 registry 凭据。
- Flutter 3.44.1，与 Dockerfile.web 一致。
- pgvector/Postgres 17 开发数据库、sqlx-cli 0.9.0、Postgres 17 客户端；设置开发 DATABASE_URL。
- Linux musl CLI 需要 cargo-zigbuild、ziglang 以及相应 rustup targets。
- Windows MSVC CLI 在 Linux 上用 cargo-xwin（`cargo install cargo-xwin`）：还需要 `clang-cl`
  与 `llvm-lib`（clang/llvm 包），首次构建会从微软 CDN 下载 CRT 与 Windows SDK 到
  `~/.cache/cargo-xwin`。目标三元组仍是 `*-pc-windows-msvc`，与 Actions 一致。
- macOS 目标需要 Apple SDK，只能在 macOS 宿主上构建（zig 只带 libc stub，不带
  `Security` / `CoreFoundation` framework，Linux 上交叉构建会在链接期失败）。

`just ci-full` = `just ci` + 数据库测试 + Flutter。`just ci` 包含 fmt、clippy、rustdoc
断链检查、workspace 单测、capture 非空测试门、两套 eval validate、Python pipeline 单测。
Actions 直接调用这些配方。数据库 job 使用 pgvector service，先 `just db-migrate`，
再 `just test-db-ready`；本地 `just test-db` 会先启动开发 compose 依赖。

不要把数据库测试连接到生产库。无数据库的检查通过不代表全绿，未运行的 target 烟测
也不能写成已验证。跨目标 CLI 只在与宿主匹配时执行版本烟测，其他目标只完成编译与打包。

## 候选协议

构建顺序：preflight → Check / Test → CLI 与两镜像 Build / smoke → Package → draft。

候选目录是 `dist/<tag>/`：

- `cormex-<version>-<target>.tar.gz` 或 `.zip`：维持 install.sh / cormex update 的文件名契约。
- 两个压缩的 Docker save 归档：cormex 和 cormex-web，都是 linux/amd64。
- `manifest.json`：记录源提交、tag、版本、CLI 目标集合以及镜像元数据。
- `SHA256SUMS`：包含 manifest 和所有要发布的构建产物的哈希。

各构建 job 的 JSON sidecar 只用于聚合。Actions 必须收到六个平台再一次生成最终
manifest 与校验表，禁止矩阵任务各自覆盖同一个 Release。六个平台为：

- x86_64-unknown-linux-musl、aarch64-unknown-linux-musl
- x86_64-apple-darwin、aarch64-apple-darwin
- x86_64-pc-windows-msvc、aarch64-pc-windows-msvc

本地保留部分 CLI 目标的能力，缺失清单写入 manifest 并警告；两个镜像缺一不可。
若需要完整六平台版本，选择 Actions 构建。不能把不同提交的 sidecar 混成一个候选。

本地构建的默认目标只有 x86_64：`x86_64-unknown-linux-musl` 与
`x86_64-pc-windows-msvc`（Linux 宿主交叉编译；macOS 宿主产出
`x86_64-apple-darwin`）。aarch64 与第二个 Apple 目标默认**不**构建，
`CORMEX_CLI_TARGETS` 是启用入口，不写在脚本里改：

```bash
CORMEX_CLI_TARGETS=aarch64-unknown-linux-musl,aarch64-pc-windows-msvc just build-local vX.Y.Z
```

单个目标可以单独补进当前候选，例如 `just build-cli-windows vX.Y.Z`、
`just build-cli-windows-arm64 vX.Y.Z`、`just build-cli vX.Y.Z <target>`。
Windows 二进制在 Linux 上不能执行，因此只完成编译、打包与 `file` 校验，
**没有版本烟测**；manifest 里也只有目标清单，别把「编出来了」写成「验证过」。

记录构建输出中的 `BUNDLE_SHA256`，它是 **SHA256SUMS 文件的 SHA-256**，不是
manifest 哈希、镜像 digest 或 tag。Actions 的聚合步骤把它写入 run summary。
确认来源、目标清单和候选内容后，把这个值带到独立的发布入口。

```bash
just release-github vX.Y.Z <构建时记录的BUNDLE_SHA256>
# 或：just release-local vX.Y.Z <构建时记录的BUNDLE_SHA256>
```

同一 tag 的候选不能覆盖。不要用 `gh release upload --clobber`、手改 checksum 或
重新打包来修复一个已经批准的候选；需要改变内容时准备新的版本/候选。

## 发布权限与顺序

Actions release.yml 使用 `production` environment。维护者应在 Settings 中设置
required reviewers，并把 DOCKERHUB_USERNAME / DOCKERHUB_TOKEN、ACR_USERNAME /
ACR_PASSWORD 放在相应 secrets 中。环境名称本身不会自动创建审批规则。
本地发布可以使用已有 Docker Hub / ACR 登录，或通过上述四个环境变量提供凭据；
脚本用 password-stdin 登录，不在命令参数中传密码。还需 gh 能读写候选。

默认仓库是 `weironz/cormex`，可用 `CORMEX_REPOSITORY` 指定目标；Actions 显式使用
当前仓库。dispatch 的工作流 ref 默认 main，可用 `CORMEX_WORKFLOW_REF` 选择已评审的
流程版本。不要在不可信的流程 ref 上注入发布凭据。

发布下载**已有 draft**，校验传入的 checksum、全部文件哈希、schema、源 tag/commit
和镜像元数据，随后 load → smoke → push → 公开 Release。发布路径不安装 Rust 或
Flutter，不调用 docker build。正式资产和 draft 中批准的是同一批字节。

镜像只使用版本 tag，从不写 latest；目标为 Docker Hub 与 ACR。cormex 检查版本及
两套 migration 目录；cormex-web 检查健康状态、真实 HTML 和编译后的 `/api` 前缀。
构建与发布都验证镜像能运行。

**镜像身份按「镜像 ID」比对，不是按 config digest。** 两套 image store 给镜像起名的
方式不同：经典 store 用 config digest 当镜像 ID，而 Docker 29 默认的 containerd
store 用 manifest digest，`docker save` 还会把 config 重写成 Docker schema，
registry 里的 config digest 又是第三个值。镜像 ID 在两种 store 下 push→pull 都不变，
所以发布只验证它：`load` 后必须报出候选记录的 ID；推送前后各拉一次注册表里的
tag（先删本地 tag），ID 必须与候选一致。候选的字节由 `SHA256SUMS` 与
`BUNDLE_SHA256` 钉住，归档内 config 不再与镜像 ID 比较 ——
那条等式只在经典 store 下成立。

npm 默认关闭。只有 `PUBLISH_NPM=true`、`NPM_TOKEN` 和完整六平台候选齐备才运行；
Actions 使用同名仓库变量与 secret。缺少 token 或完整目标集合时明确失败，不静默跳过。
发布使用候选提交中的 npm 包装文件和已校验的 CLI 归档，不重新编译，也没有单独的
未校验发布入口。每个 npm 包先打包并计算 SHA-512；同版本已存在且完整性一致就跳过，
不一致则拒绝覆盖，网络或认证失败也不会被当成「尚未发布」。上传的是刚校验的 tarball，
上传后再次核对注册表完整性。本地启用 npm 还需要 Node.js，且 git 中必须有候选提交的对象。

## 失败与重试

- 检查、任一 Actions target 或任一镜像构建失败：不产生完整候选，不发布；先修复源代码。
- 候选已存在：停止，不覆盖。核对是不是已经成功上传；不要把失败重跑当成换包授权。
- checksum、tag 或任一文件不匹配：停止发布，检查候选来源和下载完整性。
- registry 网络失败：不同 registry 的 push 无法原子提交，可能已有部分版本 tag；核对
  registry 当前状态后用同一个候选和同一个 BUNDLE_SHA256 重试，不重新构建。镜像 ID
  和 npm 包完整性与候选一致时会跳过重复上传；同版本内容不一致则停止，不自动修复。
- Release 已经公开：不要把它作为 draft 重发，也不要用新内容替换同名资产。
- 本地与 Actions 之间没有跨机器事务锁；协调操作者不要同时发布同一个 tag。Actions
  build/release 共用 candidate 并发组，且不会取消正在执行的同 tag 操作。

脚本级只读校验入口：

```bash
just _verify vX.Y.Z <BUNDLE_SHA256> dist/vX.Y.Z
python3 -m unittest discover -s scripts/tests -v
```

真实 Rust、多平台工具链、Docker、数据库、Flutter 以及线上 registry 的验证仍需在具备
这些依赖的环境运行。纯 Python 测试通过不能替代真实构建、烟测或首次发布验收。
