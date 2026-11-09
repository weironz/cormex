# 构建候选与正式发布

构建、发布是两次独立操作；每次都能独立选择本地或 GitHub Actions。
本地构建 → Actions 发布、Actions 构建 → 本地发布都走同一个候选协议。
发布不等于部署，本流程不会 ssh、修改生产配置或执行生产 migration。

## 操作入口

| 命令 | 执行地点 | 结果 |
|---|---|---|
| `just build-local vX.Y.Z` | 本地 | 全检查、构建、烟测、打包，再上传 draft candidate |
| `just build-resume vX.Y.Z` | 本地 | 重跑全检查，校验并复用已完成产物，再上传 draft |
| `just _stage vX.Y.Z` | 本地 | 仅校验并上传现有本地候选为 draft |
| `just build-github vX.Y.Z` | Actions | 全检查、六平台 CLI、两个镜像，聚合并上传 draft candidate |
| `just release-local vX.Y.Z BUNDLE_SHA256` | 本地 | 校验指定候选，推已有镜像，公开 Release |
| `just release-github vX.Y.Z BUNDLE_SHA256` | Actions | 经 production environment 发布同一候选 |

`build-github` / `release-github` 只是触发运行；触发成功不代表构建/发布成功。
发布 workflow 的手动表单使用 `tag` 和 `checksum` 两个输入，后者填构建时记录的
`BUNDLE_SHA256`。到对应 Actions run 查看结果。三个 workflow 都不接受 push / pull_request / tag 自动触发。
CI 的 `workflow_call` 只供 build 复用。历史的 `just release 0.1.14` 会失败并给出迁移提示。

## 本地操作顺序

发布操作以本文为准；目标机器的部署与恢复流程见 [部署与运维指南](deployment-guide.md#9-用-justfile--ansible-部署验收与演练)。所有 `vX.Y.Z`、摘要、主机、用户与地址均须替换为操作者确认的值。

1. `just doctor`：不需要 tag，先检查本机工具、版本一致性、GitHub 访问、当前 Docker context / engine / builder、开发数据库和磁盘空间。
2. `just prepare-version vX.Y.Z`：更新 workspace 版本、Cargo.lock 中本仓库包的版本、`app/pubspec.yaml` 和 changelog。默认增加 app build number 并使用当前日期；不创建 commit、tag，也不 push。
3. 审阅四个版本文件及本次变更，完成提交，再创建并推送指向该提交的 tag。构建要求工作树干净、HEAD 等于 tag、远端 tag 指向同一提交，版本文件彼此一致。
4. `just build-local vX.Y.Z`：每次调用都执行 `just ci-full`，之后构建、烟测、打包并上传 draft。保存输出的 `BUNDLE_SHA256`，审阅候选和目标缺失清单。
5. 确认 draft 中的资产与批准候选一致。Justfile 把构建和 stage 两个阶段依次执行；诊断时可用 `python3 scripts/release.py build-local vX.Y.Z` 只生成本地候选，再用 `just _stage vX.Y.Z` 上传。`build-github` 在 Actions 完成聚合后上传 draft。
6. 使用构建时独立记录的摘要执行 `just release-local vX.Y.Z BUNDLE_SHA256`，或 `just release-github vX.Y.Z BUNDLE_SHA256`。
7. Release 公开后，按部署指南执行 `just doctor-deploy` 与 `just deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256`，保存升级回执并验收。

准备版本的可选参数与只读预检也可直接调用脚本：

```bash
python3 scripts/release.py doctor --json
python3 scripts/release.py doctor --deployment --json
python3 scripts/release.py prepare-version vX.Y.Z --build-number N --date YYYY-MM-DD
python3 scripts/release.py preflight vX.Y.Z --profile local
```

`doctor` 只报告检查结果，不切换 Docker context、不启动 builder、不登录或安装依赖。`--deployment` 增加部署环境变量和 SSH 文件路径检查；完整本地构建依赖仍需满足。`preflight` 增加 tag、提交与工作树的约束；`--profile source` 适用于 Actions 的源码检查，Actions 默认使用此 profile，本地默认 `local`。源码 profile 的成功不能证明本机具备完整构建环境。

示意命令（仅在确认准备发布的提交后执行）：

```bash
just doctor
just prepare-version vX.Y.Z
# 审阅并提交版本文件及本次变更后：
git tag -a vX.Y.Z -m vX.Y.Z
git push origin vX.Y.Z
just build-local vX.Y.Z
# 或选择 Actions 构建：just build-github vX.Y.Z
```

包含新脚本的流程无法直接在没有脚本的历史 tag 上运行。准备版本会拒绝无关的依赖解析变更；离线 metadata 失败时恢复这四个文件，应先补齐本地依赖缓存再重试。

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

本地构建顺序：preflight → `ci-full` → CLI 与两镜像 Build / smoke → Package → stage → draft。Python 的 `build-local` 只执行到 Package；公开的 `just build-local` / `just build-resume` 随后单独调用 stage。Actions 在各 job 成功后统一聚合并 stage。

服务端 Dockerfile 对 Cargo registry、Git 与 target 使用持久 BuildKit cache。实际源码构建后把二进制导出到 `/out`，运行镜像从那里复制，避免把 cache mount 当作最终镜像层。缓存加速编译，不免除检查或镜像烟测；不要在流水线间自动 prune builder/cache。

本地构建回执为 `dist/.<tag>.build-receipt.json`，发布回执为 `dist/.<tag>.publish-receipt.json`，均在候选目录之外，记录阶段、耗时、状态与操作身份。发布回执还保存已校验的 registry 镜像 digest，便于后续追溯；它不是新的候选资产，也不能替代批准摘要。

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
`just build-cli-windows-arm64 vX.Y.Z`、`just build-cli TARGET vX.Y.Z`。
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

脚本提供默认目标仓库；其他维护者用 `CORMEX_REPOSITORY=OWNER/REPOSITORY` 指定自己的仓库，Actions 显式使用当前仓库。dispatch 的工作流 ref 默认 main，可用 `CORMEX_WORKFLOW_REF` 选择已评审的流程版本。不要在不可信的流程 ref 上注入发布凭据。

发布下载**已有 draft**，校验传入的 checksum、全部文件哈希、schema、源 tag/commit
和镜像元数据，随后 load → smoke → push → 公开 Release。发布路径不安装 Rust 或
Flutter，不调用 docker build。正式资产和 draft 中批准的是同一批字节。

镜像只使用版本 tag，从不写 latest；目标为 Docker Hub 与 ACR。cormex 检查版本及
两套 migration 目录；cormex-web 检查健康状态、真实 HTML 和编译后的 `/api` 前缀。
构建与发布都验证镜像能运行。

**镜像内容通过共享的 `scripts/image_identity.py` 比对。** Docker image ID、config digest 与 registry manifest digest 含义不同，跨经典 / containerd store 以及 save/load 转换时不能假定相等。内容指纹对平台、规范化后的运行配置和按顺序排列的未压缩 rootfs `diff_ids` 做 SHA-256；保留影响运行的配置与层顺序，忽略构建历史、时间戳等元数据。

发布分别从批准的归档、load 后的镜像及 registry 拉回的镜像计算同一内容指纹，再核对并记录各 registry 的 digest。不同存储后端可得到不同 image ID / digest，只要运行内容与批准归档一致即可；同版本 tag 已存在且内容不一致时停止，不能覆盖。

manifest 仍为 schema 1，原有候选资产保持兼容。新候选可增加 `content_sha256`，Web 镜像另可增加 `web_main_js_sha256`。旧候选缺少这些字段时，从已校验的镜像归档计算内容身份；不修改原 manifest 或校验表。`SHA256SUMS` 与 `BUNDLE_SHA256` 钉住候选字节，内容指纹用于跨存储后端确认运行内容，registry digest 用于部署时的不可变引用，三者分别核对。

npm 默认关闭。只有 `PUBLISH_NPM=true`、`NPM_TOKEN` 和完整六平台候选齐备才运行；
Actions 使用同名仓库变量与 secret。缺少 token 或完整目标集合时明确失败，不静默跳过。
发布使用候选提交中的 npm 包装文件和已校验的 CLI 归档，不重新编译，也没有单独的
未校验发布入口。每个 npm 包先打包并计算 SHA-512；同版本已存在且完整性一致就跳过，
不一致则拒绝覆盖，网络或认证失败也不会被当成「尚未发布」。上传的是刚校验的 tarball，
上传后再次核对注册表完整性。本地启用 npm 还需要 Node.js，且 git 中必须有候选提交的对象。

## 失败与重试

- 本地中断后执行 `just build-resume vX.Y.Z`（等价于 `python3 scripts/release.py build-local vX.Y.Z --resume`）。恢复要求源提交、tag、工具链及构建环境与回执一致；复用前验证已完成产物的存在性与哈希，已封装候选还要完整校验。`ci-full` 每次调用都重跑，包括 resume；回执不能跳过检查。失败阶段不会被当作成功复用。
- 检查、任一 Actions target 或任一镜像构建失败：不产生完整候选，不发布；先检查失败阶段。需要修改已打 tag 的源代码时准备新候选，不移动已批准 tag。
- 候选已存在：停止，不覆盖。核对是不是已经成功上传；不要把失败重跑当成换包授权。
- checksum、tag 或任一文件不匹配：停止发布，检查候选来源和下载完整性。
- registry 网络失败：不同 registry 的 push 无法原子提交，可能已有部分版本 tag；核对
  registry 当前状态后用同一个候选和同一个 BUNDLE_SHA256 重试，不重新构建。镜像内容指纹
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
