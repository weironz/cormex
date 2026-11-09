# Cormex 部署与运维指南

本文面向首次部署和日常维护 Cormex 的人员，覆盖单机 Compose、账号初始化、模型、加密、迁移、升级及备份恢复。客户端接入见 [AI agent 接入与使用指南](agent-integration-guide.md)，擦除和恢复演练的实现细节见 [运维手册](operations.md)。

本文按仓库当前实现编写。源码版本、已发布镜像版本、已安装 CLI 版本和正在运行的服务版本应分别核对；Git 提交或推送不会自动升级任何部署。示例域名均为占位域名，不包含现网信息。

除明确标注 PowerShell 的代码块外，多行命令使用 Bash 语法。在 PowerShell 中执行时，将带 `\` 续行的命令合并成一行；不要把 Bash 续行符原样复制到 PowerShell。

## 1. 部署组成与地址

| 组件 | 职责 | 默认对外入口 |
| --- | --- | --- |
| `postgres` | Postgres 17、pgvector、账号、租户和记忆数据 | 无宿主机端口 |
| `rustfs` | 采集分块、附件等对象 | `127.0.0.1:9000` |
| `cormex` | cormexd：HTTP、MCP、后台解析与提取 | 通过 proxy 的 `/api/` |
| `web` | Flutter 静态记忆浏览器 | 通过 proxy 的 `/` |
| `proxy` | Nginx，分流页面与 API | `127.0.0.1:8080` |
| `embeddings` | 可选的本地向量模型 | Compose 内部，profile 为 `embed` |

标准同源部署的地址对应关系：

```text
http://127.0.0.1:8080/             Web
http://127.0.0.1:8080/api/health   API 健康信息
http://127.0.0.1:8080/api/mcp      Streamable HTTP MCP
http://127.0.0.1:9000/             浏览器访问的对象存储
```

容器内使用服务名访问数据库和 RustFS。浏览器访问对象存储时，使用的是另一个可达的公开地址。两者不要混用。

## 2. 前置条件与版本选择

- 安装 Docker Engine 或 Docker Desktop，以及支持 `docker compose` 的 Compose 插件。
- 准备足够的持久化磁盘空间；历史对话、工具输出、附件和数据库备份会持续增长。
- 准备 1024 维 embedding 服务；自动提取长期记忆还需要可用的 LLM。
- 本地运行 `bge-m3` 的可选 embedding 容器约需额外 3 GB 常驻内存，模型下载和其他服务也需要余量。使用远程 embedding 时可省去该容器。
- 公网访问需配置 HTTPS、对象存储入口和准确的浏览器 origin。

从仓库根目录进入部署目录：

```bash
cd deploy
cp .env.example .env
```

PowerShell 中复制文件可使用 `Copy-Item .env.example .env`。

编辑 `.env`，将 `CORMEX_VERSION` 设为你确认已经发布、可拉取且符合需求的版本。模板内的版本号只是示例，不代表最新版本。服务端和 Web 应选择兼容的版本，不建议使用浮动标签。

先验证目标镜像可拉取：

```bash
docker compose pull cormex web
```

若需要使用尚未发布的源码，在仓库根目录自行构建：

```bash
docker build -f scripts/docker/Dockerfile.cormexd -t cormex-local:source .
docker build -f scripts/docker/Dockerfile.web -t cormex-web-local:source .
```

然后在 `deploy/.env` 中指定：

```dotenv
CORMEX_VERSION=source
CORMEX_IMAGE=cormex-local:source
CORMEX_WEB_IMAGE=cormex-web-local:source
```

本地构建方式启动时不再执行 `docker compose pull cormex web`。构建过程仍需能获取依赖和基础镜像。

## 3. 凭据与初始化账号

### 3.1 基础配置

按选定版本生成旧式共享 token 及其哈希。首次 `.env` 仍为空时，直接使用镜像，避免 Compose 在解析必填配置时就报错：

```bash
# 将 VERSION 替换为选定的真实镜像标签。
# 将 REGISTRY/NAMESPACE 替换为选定发布镜像的仓库。
docker run --rm REGISTRY/NAMESPACE/cormex:VERSION --generate-token
```

把输出的哈希填入 `CORMEX_AUTH_TOKEN_SHA256`；token 原文保存在客户端密钥管理工具中，不要写入文档、Git 或公开日志。这个 token 与账号登录后签发的设备密钥是不同的凭据。

`.env` 至少需要填写：

```dotenv
CORMEX_VERSION=VERSION
CORMEX_AUTH_TOKEN_SHA256=REPLACE_WITH_GENERATED_HASH
POSTGRES_PASSWORD=REPLACE_WITH_DATABASE_PASSWORD
RUSTFS_SECRET_KEY=REPLACE_WITH_OBJECT_STORE_SECRET
```

替换所有占位值。数据库密码会被拼进连接 URL；若包含 `@`、`:`、`/`、`#` 等 URL 特殊字符，应改用正确编码的连接串或选择随机的 URL 安全密码，避免把连接错误误判成数据库故障。

### 3.2 必须显式传入容器的扩展配置

当前 `deploy/docker-compose.yml` 对环境变量使用显式列表。只在 `.env` 中新增变量，并不意味着 cormexd 能收到它。管理员账号、主加密密钥和下列自动记忆选项需要扩展配置。

在 `deploy/` 创建本地 `compose.override.yaml`：

```yaml
services:
  cormex:
    environment:
      CORMEX_ADMIN_USERNAME: ${CORMEX_ADMIN_USERNAME:?set administrator username}
      CORMEX_ADMIN_PASSWORD: ${CORMEX_ADMIN_PASSWORD:?set administrator password}
      CORMEX_SECRET_KEY: ${CORMEX_SECRET_KEY:?set a 32-byte hexadecimal key}
      CORMEX_OPEN_REGISTRATION: "false"
      CORMEX_LLM_BASE_URL: ${CORMEX_LLM_BASE_URL:-}
      CORMEX_PROMOTE_ENABLED: ${CORMEX_PROMOTE_ENABLED:-false}
      CORMEX_PROMOTE_PER_ROUND: ${CORMEX_PROMOTE_PER_ROUND:-40}
      CORMEX_DISTILL_DAILY_TOKENS: ${CORMEX_DISTILL_DAILY_TOKENS:-200000}
```

后续命令均从 `deploy/` 执行，并固定带上两个文件：

```bash
docker compose -f docker-compose.yml -f compose.override.yaml config --quiet
```

也可以使用本地 shell 函数缩短命令，但不要在某次升级时漏掉 override。不要公开未脱敏的 `docker compose config` 输出，其中会展开凭据。

在 `.env` 增加账号及密钥：

```dotenv
CORMEX_ADMIN_USERNAME=REPLACE_WITH_ADMIN_USERNAME
CORMEX_ADMIN_PASSWORD=REPLACE_WITH_STRONG_ADMIN_PASSWORD
CORMEX_SECRET_KEY=REPLACE_WITH_64_HEX_CHARACTERS
```

生成主密钥：

```bash
openssl rand -hex 32
```

没有 OpenSSL 的 PowerShell 可使用：

```powershell
$bytes = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($bytes)
$rng.Dispose()
([BitConverter]::ToString($bytes)).Replace('-', '').ToLowerInvariant()
```

账号初始化只在整个账号库为空时执行。修改 `.env` 后重启不会重置已有账号密码。账号创建失败也不一定导致整个进程退出，因此需要检查日志并实际登录；不能仅以容器运行中判断账号可用。

### 3.3 加密密钥保管

`CORMEX_SECRET_KEY` 用于采集数据密钥和用户自配模型凭据等敏感数据的加密。配置后应通过健康信息确认 `capture_encryption` 为 `encrypted`。

- 新采集分块启用加密；已有明文分块不会自动转换。
- 主密钥须单独、受控备份。仅有数据库和对象副本仍不足以恢复加密内容。
- 随意换成新密钥会导致旧加密内容不可读，不是普通的密码轮换。
- 旧数据库备份可能保留被包裹的数据密钥；擦除和备份保留策略需一起设计，见 [运维手册](operations.md)。

## 4. 配置 embedding 与长期记忆提取

### 4.1 远程 embedding

在 `.env` 设置：

```dotenv
CORMEX_EMBED_BACKEND=api
CORMEX_EMBED_ENDPOINT=https://embedding.example.com/v1/embeddings
CORMEX_EMBED_MODEL=REPLACE_WITH_1024_DIMENSION_MODEL
CORMEX_EMBED_API_KEY=REPLACE_IF_REQUIRED
```

端点必须提供兼容请求，模型输出必须是 1024 维，才能匹配当前 `VECTOR(1024)` schema。不能仅凭“支持 embeddings”就认为模型可用。健康信息报告模型配置，也不能替代一次真实向量化或语义搜索验收。

### 4.2 本地 embedding

改为：

```dotenv
COMPOSE_PROFILES=embed
CORMEX_EMBED_BACKEND=api
CORMEX_EMBED_ENDPOINT=http://embeddings/v1/embeddings
CORMEX_EMBED_MODEL=bge-m3
```

首次会下载模型。观察 `embeddings` 日志，待服务可用后再验收语义检索。

### 4.3 LLM 和自动记忆

例如使用部署模板已传入的 DeepSeek 配置：

```dotenv
CORMEX_LLM_PROVIDER=deepseek
DEEPSEEK_API_KEY=REPLACE_WITH_PROVIDER_KEY
# 如需覆盖默认模型，再设置以下值。
#CORMEX_LLM_MODEL=YOUR_MODEL_ID
#CORMEX_LLM_CHEAP_MODEL=YOUR_EXTRACTION_MODEL_ID
CORMEX_PROMOTE_ENABLED=true
```

若需要自定义端点，使用与所选 provider 兼容的 `CORMEX_LLM_BASE_URL`。其他 provider 的模型名、凭据要求以该 provider 实现和实际服务契约为准。

自动提取是否启用还受账号级“记忆提取设置”控制。账号一旦保存过设置，不能假设修改服务器默认开关就会覆盖该账号。登录 Web，在设置页检查提取开关、模型连接测试和处理状态。

当前状态包括 `paused`、`needs_configuration`、`queued`、`processing`、`caught_up`、`error` 等。归档有数据而长期记忆为空时，先查这些状态，而不是反复上传。

`CORMEX_DISTILL_DAILY_TOKENS` 是蒸馏路径的预算，不应当当作全部 LLM 调用的统一消费上限；`0` 表示不限制。`CORMEX_PROMOTE_PER_ROUND=0` 会关闭该晋升路径。试运行、主动记忆写入和其他模型调用需分别核算。

## 5. 启动、迁移和验收

```bash
docker compose -f docker-compose.yml -f compose.override.yaml config --quiet
docker compose -f docker-compose.yml -f compose.override.yaml up -d
docker compose -f docker-compose.yml -f compose.override.yaml ps
docker compose -f docker-compose.yml -f compose.override.yaml logs --tail 100 cormex
curl -fsS http://127.0.0.1:8080/api/health
```

服务启动路径会执行迁移；新建账号还会初始化对应租户。迁移由 SQLx 版本表记录，重复启动不会重复建表。

验收时逐项确认：

1. 健康响应标识 `role=cormexd`，版本与目标镜像一致。
2. 日志表明使用真实 Postgres 后端；不要使用 `CORMEX_BACKEND=mock` 作为生产故障回退。
3. 账号可登录，日志没有 `/auth/* will answer 501` 或管理员初始化失败。
4. `capture_encryption=encrypted`，embedding 模型配置与预期一致。
5. 一台客户端可登录、同步一条测试会话；Web 可读其正文，MCP 可搜索该会话。
6. 开启提取时，账号模型连接测试成功，处理状态能够前进。

### 对象存储属于本项目

自足模板把 `rustfs` 服务和 `rustfs_data` 卷放在同一个 Compose 项目中；生产参考模板另有 `${S3_DOMAIN}` 路由。部署前检查容器名、卷与路由归属，避免误用其他产品的数据或把同一域名指向两个独立对象存储。

浏览器取 blob 走**预签名 URL**，签名里包含 host。为当前对象存储配置独立可达的入口，例如 `https://objects.example.com`，并核对其桶、权限和 CORS。

历史部署迁移对象存储时，先确认源卷和目标卷，停写后复制并校验，再切换端点。下面两个卷名都是占位值，不可直接照抄；底层文件复制还需停止对应对象存储，完整步骤见第 8 节。

```bash
docker volume create REPLACE_WITH_TARGET_VOLUME
docker run --rm -v REPLACE_WITH_SOURCE_VOLUME:/from:ro -v REPLACE_WITH_TARGET_VOLUME:/to \
  pgvector/pgvector:pg17 sh -c 'cp -a /from/. /to/'
```

`depends_on` 只能引用同一个 Compose 项目里的服务，而且 `service_started` 不等于「可读」。cormexd 在启动时连接对象存储，不可达时可能 warning 后回落到本地文件系统。可以在本地 override 增加等待，减少启动时序问题：

```yaml
services:
  cormex:
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        until code=$$(curl -s -o /dev/null -w "%{http_code}" "$${S3_ENDPOINT}"); [ "$$code" != "000" ]; do sleep 2; done
        exec /usr/local/bin/cormexd
```

判定「可达」看状态码不是 2xx —— S3 根路径对匿名请求回 403，那已经证明服务在。
`just deploy` 另外断言 `/api/health` 的 `blob_backend == s3`，所以即使等待失效，
回落到本地文件系统的部署也会被判失败。

### 迁移约束

- `migrations/` 与 `migrations-global/` 有各自版本表；全局认证 schema 和租户 schema 不应合并。
- 启动迁移代码处理全局与租户依赖，以及历史 `cortex_auth` 名称兼容；优先使用正常启动路径。
- 维护环境要手动执行时，先读 `justfile` 的 `db-migrate`，不要把两个目录任意调换执行。
- 新 schema 变更新增 migration；已应用文件的内容和 LF 行尾不可修改。
- 出现 checksum 错误，比较文件与镜像字节；不要修改数据库中的 checksum 来跳过校验。
- 新版服务可能增加数据库结构，旧镜像不一定兼容。启动前备份，升级失败时按实际 schema 判断恢复方案。

## 6. HTTPS、反向代理与对象存储

远程部署应为 Web/API 提供 HTTPS，例如 `https://memory.example.com`，并为浏览器访问的对象存储提供可达 HTTPS 地址，例如 `https://objects.example.com`。

`.env` 中设置：

```dotenv
CORMEX_PUBLIC_URL=https://memory.example.com
CORMEX_S3_PUBLIC_URL=https://objects.example.com
```

外层代理可转发到回环监听的 `8080` 和 `9000`，保持数据库不发布宿主机端口。若代理位于另一容器中，不能把那个容器自己的 `127.0.0.1` 当作宿主机地址；需配置实际 Docker 网络或可达入口。

反代必须满足：

- `/api/` 转发到 API，并仅剥离一次 `/api` 前缀；其他请求进入 Web。
- 传递 `Authorization`，支持 MCP 的流式 HTTP、SSE 和 `/sync` WebSocket。
- 关闭流式响应缓冲，设置足够的读写超时，配置合适的上传大小限制。
- 对象存储 CORS 允许准确的 Web origin；HTTPS 页面不要指向 HTTP 对象入口。
- 保留预签名请求的 host、路径和查询参数，不任意增加或删除路径前缀。

先核对仓库的 [proxy.conf](../deploy/proxy.conf)。Web 本身不反代 API；把 `/api` 请求送到 Web 会得到 HTML，即使 HTTP 状态是 200 也不正确。

同源 Web/API 通常不需要额外开放 API CORS。确需跨源时，用 `CORMEX_CORS_ALLOWED_ORIGINS` 配置准确 origin，并通过 override 显式传入服务容器。

`docker-compose.prod.yml` 是特定部署的参考，包含固定 external 卷等假设，不能直接当成一台新机器的安装模板。共享 Docker 守护进程上还要检查固定 `container_name`、端口和项目名冲突。

## 7. 升级与回退

已按第 9 节接入 Ansible 的部署，统一使用 `just deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256`；备份、pin 更新和严格验收由该入口执行。`just deploy TAG` 只重应用目标机器已经 pin 的版本，不负责切换版本。完整构建、上传和发布顺序见 [发布手册](release.md#本地操作顺序)。

独立维护的自足部署也可以手工升级，但需自行完成以下步骤与第 5 节验收：

1. 记录当前服务、Web、CLI 版本和使用的 Compose 文件、卷名。
2. 备份数据库、对象存储和主加密密钥，确认恢复流程可用。
3. 阅读变更与 migration，选定已发布镜像或自行构建。
4. 更新 `.env`，拉取镜像，使用相同文件和项目名启动。

```bash
docker compose -f docker-compose.yml -f compose.override.yaml pull cormex web
docker compose -f docker-compose.yml -f compose.override.yaml up -d cormex web
docker compose -f docker-compose.yml -f compose.override.yaml logs --tail 100 cormex
```

本地构建镜像时省去 pull。升级后重新执行第 5 节验收。CLI 另行升级，并检查 `cormex --version`；镜像升级不影响客户端可执行文件。

仅当数据库结构仍兼容时，才能直接切回旧镜像。需要数据库回退时，应在隔离环境恢复升级前备份，连同对应对象副本和密钥一起验证，再安排切换。

不要用 `docker compose down -v` 进行普通升级；它会删除此 Compose 管理的数据卷。更换项目名或错误改写 external 卷也可能得到全新空库。

## 8. 备份与恢复

### 8.1 备份范围

完整备份应包含全部租户及认证 schema 的数据库、对象存储、主加密密钥、版本记录和部署配置。不要只备份 `public`；后创建的账号可能使用单独 schema。

两套 compose 模板都**不启用**连续 WAL 归档（PITR）。原因见 `docker-compose.yml` 中 postgres 段的注释：`/backup` 是 Docker 以 root 创建的 bind mount，而归档进程以 `postgres`(uid 999) 运行，命令必然失败；归档失败的 WAL 段永不回收，磁盘会被写满。定期 dump 提供的是备份时点恢复能力；如果确实需要任意时间点恢复，必须同时让备份目录对 uid 999 可写、开启 `archive_mode`、监控 `pg_stat_archiver.failed_count`，并执行恢复演练。

下面使用容器内生成 dump 再复制的方式，避免旧版 PowerShell 对二进制重定向的处理差异：

```bash
docker compose -f docker-compose.yml -f compose.override.yaml exec -T postgres sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Fc -f /tmp/cormex-backup.dump'
docker compose -f docker-compose.yml -f compose.override.yaml cp postgres:/tmp/cormex-backup.dump ./cormex-backup.dump
```

每次使用独立、带时间的宿主机备份目录，避免覆盖唯一副本。dump 包含用户数据，应受控存储。

自动升级会生成数据库 dump、摘要与部署配置副本，并通过 `pg_restore --list` 验证备份目录可读。这不能证明数据已成功恢复，也不包含对象存储快照；对象的一致副本、主密钥保管、异机备份和第 8.2 节恢复演练仍须独立安排。

对象存储可使用其支持的复制、备份工具；若直接归档 RustFS 的底层卷，应安排停写并取得一致快照，不能假设对活跃卷执行一次 `tar` 就有完整一致性。可在维护窗口停止 `cormex`，完成数据库备份后停止 RustFS，再归档其数据卷；备份完成后启动 RustFS 和 `cormex`。同时暂停外部写入对象存储的调用方。

先核实实际卷名：

```bash
docker inspect cormex-postgres --format '{{json .Mounts}}'
docker inspect cormex-rustfs --format '{{json .Mounts}}'
```

卷名以当前部署为准，不照抄另一台机器的历史名称。主密钥与数据备份分开保管。

### 8.2 恢复演练

在隔离主机或完全独立的容器、卷和端口中恢复。当前模板有固定容器名，仅修改 Compose 项目名并不足以同时运行第二套。

1. 准备兼容版本的 Postgres/pgvector 和 RustFS，使用新的空数据库及对象卷。
2. 对完整 schema dump，恢复到空库；无需先运行应用迁移再恢复同一份表定义。
3. 用匹配服务端大版本的 `pg_restore`，启用遇错退出；远程托管数据库需另行处理权限和扩展安装。
4. 恢复一致的对象副本，并配置原主密钥。
5. 使用备份对应版本启动服务，再按需升级；比对租户、会话、分块和记忆记录数。
6. 实际阅读加密会话、附件、历史检索及 MCP 响应，证明对象与数据库能对上。
7. 根据备份之后保存的擦除记录，重放 purge/redact；这些记录必须来自备份之外仍可用的审计或增量来源。旧快照本身不知道之后发生过什么。

备份是否有效，以恢复演练为准。自动化数据库恢复测试和 RustFS 卷演练入口见 [运维手册](operations.md)。

## 9. 用 Justfile + Ansible 部署、验收与演练

前面的步骤都可以手工执行；仓库还提供了一组配方，做的是同一件事，只是把「版本对不对」
变成会被检查的断言。**发布不等于部署**：这些配方不构建、不打 tag、不碰候选，只拉已发布
的镜像。

部署通过 **Ansible over SSH** 完成，入口仍是 `just`。**清单入库、值只从环境变量来**：
`deploy/ansible/inventory.yml` 是仓库里唯一的一份清单，主机、用户、目录、密钥路径全部来自
环境变量，没有本地 env 文件可回落——少一个必需项就直接失败，不会拿默认值猜。

| 场景 | 值从哪来 |
|---|---|
| GitHub Actions | repository secrets（同名） |
| 本地 Linux / macOS / WSL | shell 或系统环境变量 |
| Windows（Git Bash） | 系统环境变量（`setx`），playbook 在容器里跑 |

```bash
export DEPLOY_HOST=REPLACE_WITH_HOST
export DEPLOY_USER=REPLACE_WITH_SSH_USER
export DEPLOY_DIR=/REPLACE_WITH_STACK_DIRECTORY
export DEPLOY_API=https://memory.example.com
export DEPLOY_SSH_KEY=/REPLACE_WITH_PRIVATE_KEY_PATH
export DEPLOY_KNOWN_HOSTS=/REPLACE_WITH_KNOWN_HOSTS_PATH
just doctor-deploy
just deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256   # 备份、切版本、验收
just deploy TO_TAG                                 # 重应用已经 pin 的版本
just deploy-check                                  # 只做 playbook 语法检查
```

| 变量 | 必需 | 说明 |
|---|---|---|
| `DEPLOY_HOST` | 是 | 节点地址 |
| `DEPLOY_USER` | 是 | 登录用户 |
| `DEPLOY_DIR` | 是 | 目标主机上的部署目录（compose + `.env`） |
| `DEPLOY_API` | 是 | 栈在该主机上的访问地址（公网入口也可以），不包含 `/api` 后缀 |
| `DEPLOY_PORT` | | 默认 22 |
| `DEPLOY_SSH_KEY` | | 本机私钥**路径**；不设则用 ssh 自己的默认（`~/.ssh/id_ed25519`、agent、ssh_config） |
| `DEPLOY_KNOWN_HOSTS` | | known_hosts **路径**；不设则用 ssh 默认的 `~/.ssh/known_hosts` |
| `DEPLOY_PROJECT` | | compose 项目名，仅当与文件里的 `name:` 不同 |
| `DEPLOY_CONNECTION` | | 默认 `ssh`；本机演练设 `local` |
| `DEPLOY_PULL` | | 重应用模式用 Release 归档 `docker load` 后可设 `false`；升级模式必须保持拉取与校验 |
| `DEPLOY_REQUIRE_ENCRYPTION` | | 默认 `true`；仅为明确保留明文的测试栈设 `false`，不会关闭其他验收 |

缺任何一个必需项时 wrapper 会直接报出缺的名字，例如：

```text
deploy: missing environment variable(s): DEPLOY_USER DEPLOY_DIR -- export them (docs/deployment-guide.md, section 9)
```

**密钥只留在机器上。** 本地路径直接交给 `ansible_ssh_private_key_file`；Windows 走容器时，
那把私钥以只读方式挂进 `/root/.ssh/deploy_key`，`known_hosts` 同样只读并挂载，镜像里
没有任何凭据。`host_key_checking` 保持开启。

Actions 侧用 `.github/workflows/deploy.yml`（手动触发，`environment: production`）：

| 名字 | 类型 | 内容 |
|---|---|---|
| `DEPLOY_HOST` | secret | 节点地址 |
| `DEPLOY_USER` | secret | 部署用户 |
| `DEPLOY_SSH_KEY` | secret | 私钥全文（job 写入 `$RUNNER_TEMP`，0600） |
| `DEPLOY_KNOWN_HOSTS` | secret | 主机公钥行（缺失即失败，不允许 TOFU） |
| `DEPLOY_DIR` / `DEPLOY_API` | secret | 部署目录与访问地址 |

注意两边同名但形态不同：**secret 里放内容，环境变量里放路径**（`DEPLOY_SSH_KEY`、
`DEPLOY_KNOWN_HOSTS`）。job 会把内容落成文件再把同名变量指过去。本地 `doctor-deploy` 会检查这两个路径指向现存文件；部署 wrapper 的 SSH 默认路径行为仍按上表。

workflow 只部署**已公开的 Release**（先 `gh release view` 查 draft），不构建、不打 tag，
`tag` 为目标版本。可选输入 `from_tag` 和 `checksum` 均为空时，调用 `just deploy TAG` 重应用目标已经 pin 的版本；升级时须同时填写当前旧版本与构建时批准的 `BUNDLE_SHA256`，调用 `just deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256`。只填其中一项会失败，两种模式执行同一份严格验收。Actions 部署串行执行。

### 受控升级与回执

`just deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256 [inventory]` 使用构建时记录并批准的摘要。`FROM_TAG` 与 `TO_TAG` 均是带 `v` 的真实版本；可选清单默认 `deploy/ansible/inventory.yml`。控制端先确认目标 Release 已公开，下载候选，校验明确传入的 `BUNDLE_SHA256`、全部资产、源提交和镜像内容，将批准内容指纹与允许的 registry 仓库交给目标主机。目标主机使用当前配置的 registry 拉取目标 tag，先核对内容指纹，再记录其 `RepoDigest` 并以 digest 固定启动；控制端无需同时连接各镜像 registry。不得从远端现读一个摘要作为批准值。

Ansible 把仅依赖 Python 标准库的部署辅助脚本和校验后的预期值复制到目标机器，再执行以下阶段：

1. 加部署锁，确认 `.env` pin 与运行版本均等于 `FROM_TAG`，记录当前挂载与数据库统计。
2. 在栈目录的 `release-backups/` 下创建私有备份目录，保存部署配置、数据库 dump、摘要及 `pg_restore --list` 结果。
3. 原子替换 `.env` 中的 `CORMEX_VERSION`，为目标 tag 准备配置，检查项目名、存储服务、卷及镜像之外的应用配置保持一致。
4. 从现有配置指向且在允许范围内的 registry 拉取两个应用镜像，校验其可移植内容指纹，记录 `RepoDigest` 并原子写入 `compose.release.json`。按 digest 执行 `up -d --no-deps cormex web`，不重建数据库或对象存储容器。
5. 使用与本地验收相同的严格检查，确认升级完成并记录前后挂载。

每个阶段的开始、结束、耗时和状态写入备份目录内的 `deploy-receipt.json`；执行输出给出 `DEPLOY_RECEIPT` 路径。回执含主机侧状态，配置副本和 dump 含敏感信息，应私有保管，勿上传到公开 issue、Release 或仓库。

只有**尚未尝试启动新容器**时的失败，才自动恢复旧 `.env` 和旧 digest overlay。开始调用 `up` 后，即使命令失败，也可能已经运行 migration；此时保留现场与失败回执，由操作者检查 schema 和备份后决定修复、重试或恢复。流程不自动回滚数据库，也不保证切回旧镜像即可恢复。涉及对象或密钥时还需第 8 节的对应副本。

`just deploy TAG` 仍要求目标 `.env` 已经 pin 为该 tag，否则停止；它重应用已 pin 版本并执行共享验收，不替代上述升级入口。自动化 Compose 调用加载存在的 `docker-compose.override.yml`、`compose.override.yaml` 和 `compose.release.json`；日后手工操作也必须带齐，避免漏掉主密钥或 digest pin。

### 共享验收的含义

- `/api/health` 必须是 JSON，`role=cormexd`、版本匹配，服务及数据库状态均为 `ok`，`blob_backend=s3`，采集加密为 `encrypted`。
- 服务端、Web、Postgres 和 RustFS 容器均须运行且 health 为 `healthy`。升级路径还核对两个应用镜像的批准内容和 registry digest，并比较升级前后的持久化挂载。
- Web 根必须是 HTML；`/version.json` 版本必须与目标一致。公网 `/main.dart.js` 的 SHA-256 必须等于运行 Web 容器内文件的 SHA-256；候选提供 `web_main_js_sha256` 时还与候选比对，防止 CDN 或代理仍返回旧页面脚本。
- 表统计使用 `pg_stat_user_tables.n_live_tup`，它是估计值。回执在前后使用同一方法记录，供排障参考；统计滞后或并发写入均会影响结果，不能当作精确记录数，也不能证明没有数据丢失。恢复验证仍要实际查询数据并读取对象。

`just deploy-check` 只验证 playbook 语法，不连接主机、不拉镜像、不执行以上运行时验收。`doctor-deploy` 也不代表升级已完成。

**Windows 不能直接当 Ansible 控制节点。** `ansible-playbook` 只在 POSIX 上运行，Windows
只是被管理端（走 WinRM，与本项目无关）。因此 `just deploy` 在 Windows 上用
`deploy/ansible/Dockerfile` 构一个固定版本的 ansible-core 容器来执行，Linux / macOS / WSL
上直接调用本机的 `ansible-playbook`。两条路径的入口、参数与验收完全一致。

本机自测（不需要远端）：

```bash
DEPLOY_HOST=localhost \
DEPLOY_USER=REPLACE_WITH_LOCAL_USER \
DEPLOY_CONNECTION=local \
DEPLOY_DIR=$PWD/.deploy-rehearsal \
DEPLOY_API=http://127.0.0.1:18080 \
DEPLOY_PROJECT=cormex-rehearsal \
just deploy vX.Y.Z
```

### 本地演练

`deploy-rehearsal` 用同一份 `deploy/docker-compose.yml`、同两个镜像，在**隔离的 compose
项目**上跑一遍：独立的卷（`cormex-rehearsal_*`）、自己的容器名、端口 18080 与 19000，
凭据随机生成并写入 `.deploy-rehearsal/.env`（0600，已 gitignore）。它验证的是真实镜像 +
真实 compose 文件 + 全新数据库的迁移路径，不会碰开发栈或任何已有数据。

```bash
just deploy-rehearsal vX.Y.Z   # 生成凭据、拉镜像、起栈、验收
just deploy-verify .deploy-rehearsal
just deploy-rehearsal-down     # 停栈；卷和凭据保留，再跑同一版本会复用
```

演练只保存 token 的 **SHA-256**，不落明文（登录用生成的 `CORMEX_ADMIN_USERNAME` /
`CORMEX_ADMIN_PASSWORD`）。它不能替代生产验收：HTTPS、域名、对象存储 CORS、真实客户端
同步与提取状态仍要在目标环境核对。

## 10. 常见故障

| 现象 | 优先检查 |
| --- | --- |
| 登录 405、JSON 解析出 HTML | API base 是否包含正确前缀，反代是否误送到 Web |
| 页面可开但登录 501 | 全局账号库连接、迁移顺序、账号初始化日志 |
| 容器反复退出 | 数据库地址、密码、真实后端连接、迁移 checksum |
| 主密钥配了仍显示 plaintext | override 是否实际加载，变量是否进入 cormex 容器 |
| 记忆为空但会话可见 | 账号提取开关、模型配置、处理状态与错误、预算 |
| 向量化失败 | endpoint、密钥、模型维度是否为 1024 |
| 附件加载失败 | 对象公开地址、HTTPS、CORS、预签名地址是否被改写 |
| 升级后数据似乎全部消失 | 项目名、卷挂载、数据库名、账号租户是否改变 |
| `Bind for 127.0.0.1:18080 failed: port is already allocated`，但 `docker ps` 里没有占端口的容器 | 上一次失败的 `up` 留下的 `docker-proxy`：`sudo ss -ltnp 'sport = :18080'`，确认是 docker-proxy 后 `sudo kill <pid>`；先 `just deploy-rehearsal-down` 清掉半启动的容器再重试 |
| 旧加密会话突然不可读 | 原主密钥是否丢失或被替换，数据库与对象备份是否对应 |

## 11. 配置与实现入口

- [自足部署模板](../deploy/docker-compose.yml) 与 [环境变量模板](../deploy/.env.example)
- [服务配置解析](../crates/cormex-core/src/config.rs)
- [启动与账号引导](../crates/cormexd/src/main.rs)
- [账号级记忆设置](../crates/cormexd/src/memory_settings.rs)
- [数据库迁移不变性测试](../crates/cormex-store/tests/migrations_are_immutable.rs)
- [CLI 连接与发现协议](cli-connection.md)
