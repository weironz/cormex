# 运维手册

面向维护 cormex 部署的人。**这里不写节点地址、域名、账号** —— 那是
`deploy/PRODUCTION.local.md` 的事（这个文件是公开仓库的一部分）。

---

## 一、采集数据的加密（P6-1）

### 现在的状态怎么查

```bash
curl -s https://<你的域名>/api/health | jq .capture_encryption
# "encrypted"  或  "plaintext"
```

`cormex doctor` 也会报这一项。启动日志里同样有一行。

**这一项必须能一眼看到。** 一个以为自己加密了、实际是明文的部署，比一个知道自己是
明文的部署更危险 —— 后者至少知道自己该做什么。

### 打开它

```bash
openssl rand -hex 32     # 写进 .env 的 CORMEX_SECRET_KEY
docker compose up -d cormex
```

打开之后：

- **新写入**的分块用 AES-256-GCM 加密，每个采集文件一把数据密钥，数据密钥用
  `CORMEX_SECRET_KEY` 包裹后存在 `capture_keys` 表里
- **旧分块不迁移**，继续以明文存在，继续可读。它们的 `storage_key` 里没有 key id，
  读取路径据此区分
- 对象存储里的键变成 `{tenant}/capture/{key_id}/aa/bb/{sha}.zst.enc`

### ⚠️ 限制：备份保留期之内，crypto-shredding 是不完整的

这是**必须知道**的一条，不是实现缺陷：

- 数据密钥是**被包裹**后存在数据库里的。一份被偷走的数据库备份里没有可用的密钥 ——
  前提是 `CORMEX_SECRET_KEY` 不在同一份备份里（它只在 `.env`，不在数据库）
- **但是**：旧的数据库备份里仍然有当时那些被包裹的密钥。如果
  `CORMEX_SECRET_KEY` 没有轮换过，那些备份里的密钥仍然能解开当时的密文
- 所以：**在旧备份过期之前，「删掉这个会话」不等于「这个会话的字节在任何地方都
  读不出来」**

要缩短这个窗口，只有两条路：缩短备份保留期，或者轮换 `CORMEX_SECRET_KEY`（轮换
会让**所有**旧分块不可读 —— 那是一次性的、全量的数据丢失，不是常规操作）。

### 轮换 `CORMEX_SECRET_KEY` 的后果

**所有已加密的分块都不可读。** 解密会失败，错误是
`the data key cannot be unwrapped — most likely CORMEX_SECRET_KEY changed`。

这不是可以「先换再补」的操作。真要轮换，先确认那些数据可以丢。

### 没有配置 `CORMEX_SECRET_KEY` 时

分块保持明文。**不会生成临时密钥** —— 那会让重启之后所有旧分块变成解不开的字节，
症状是「昨天还好好的，今天全读不出来」。

---

## 一·五、擦除一个采集会话（P6-2）

```bash
curl -X DELETE "https://<域名>/api/archive/sessions/<session_id>?confirm=<session_id>" \
  -H "Authorization: Bearer <token>"
```

两道闸：`raw:read` scope，以及 `?confirm=` 必须重复目标 id。

### 顺序与崩溃

```
1. 销毁数据密钥  → 对象立刻是随机字节
2. 删对象
3. 删 P 层（事件、投影、索引块）
4. 删派生行（summaries、derivations、晋升出的 episodes）
5. 写墓碑        ← 最后
```

墓碑最后写是有意的：中途崩溃留下的是「部分消失、但仍被列出」的会话，运维看得见、
能接着做。反过来会留下「看起来已擦除、字节还在」，而那样就再没人会看第二眼。

### 它现在**会**清理抽出来的事实

purge 对每个晋升出的 episode 跑一遍 redact 级联：清 episode 正文、清派生 fact 的
`statement` / `tsv` / `embedding`（向量可近似反演原文，必须清）、追加
`fact_events` 的 `invalidate`、删派生 summary、清档案侧的事件与索引块。

**晋升出的 episode 行本身不删**，只清空正文。`facts.source_episode_id` 是
`NOT NULL REFERENCES episodes(id)` 且没有 `ON DELETE CASCADE` —— 删掉一个已经抽出
过事实的 episode 会直接违反外键，而「抽出过事实」正是晋升的常态。

### 单独擦除一条记忆

```bash
curl -X POST "https://<域名>/api/episodes/<episode_id>/redact?confirm=<episode_id>" \
  -H "Authorization: Bearer <token>"
```

同样两道闸：`raw:read` 与 `?confirm=`。响应里逐项报告清了哪些落点。

### 响应里的 `objects_failed`

如果非 0，说明有对象没删掉。这是 purge 唯一可能「部分成功」的地方，而且**不会
被静默忽略** —— 响应里如实报出来。重跑同一个请求是安全的（幂等）。

---

## 二、备份与恢复演练（P6-3）

> **状态：步骤已写，尚未在真实部署上演练过。** 演练一次之后把日期和结果补在这里。

### 备份

```bash
# 1. 数据库
pg_dump --format=custom --file=cormex-$(date +%F).dump "$DATABASE_URL"

# 2. 对象存储（RustFS 的卷，或 S3 兼容端点的同步）
#    卷是 external 且写死了名字，见 deploy/PRODUCTION.local.md
docker run --rm -v cormex_cormex-prod-rustfs:/data -v "$PWD":/backup \
  alpine tar czf /backup/rustfs-$(date +%F).tar.gz -C /data .
```

**两份都要。** 只有数据库的话，每一行 `capture_chunks` 都指向一个不存在的对象，
而症状是「会话列表正常，点进去全是空的」。

### 恢复到一个空库

```bash
# 1. 空库 + 两套 migration（顺序不能反）
createdb cormex_restore
DATABASE_URL=…/cormex_restore just db-migrate

# 2. 数据
pg_restore --dbname=cormex_restore --clean --if-exists cormex-YYYY-MM-DD.dump

# 3. 对象
docker run --rm -v cormex_restore_rustfs:/data -v "$PWD":/backup \
  alpine tar xzf /backup/rustfs-YYYY-MM-DD.tar.gz -C /data
```

### 对上记录数

```sql
-- 两边各跑一次，数字必须一致
SELECT 'episodes' AS t, count(*) FROM episodes
UNION ALL SELECT 'facts', count(*) FROM facts
UNION ALL SELECT 'capture_sessions', count(*) FROM capture_sessions
UNION ALL SELECT 'capture_chunks', count(*) FROM capture_chunks
UNION ALL SELECT 'transcript_events', count(*) FROM transcript_events;
```

对象数也要对：

```bash
find /path/to/rustfs -name '*.zst*' | wc -l
```

### 重放删除

**恢复之后必须重放 purge 墓碑。** 备份是某个时刻的快照，那之后被擦除的内容仍然在
备份里 —— 恢复等于把它们又装回来了。

```sql
-- 备份之后发生的擦除
SELECT target_kind, target_id, mode, created_at
  FROM redactions
 WHERE created_at > '<备份时刻>';
```

对每一条重新执行删除（删对象、删派生行、销毁数据密钥）。**这一步没有自动化**：
purge 的执行路径需要按 `target_kind` 分派，而把它写成一条 SQL 会让「擦除」变成
一件可以在 psql 里随手做的事 —— 那正是这个项目一直避免的。

---

## 三、部署的形状（提醒）

三件在生产上会咬人的事，细节见 `CLAUDE.md`：

- **卷是 `external` 且写死了名字**。去掉 external 或改项目名，compose 会新建空卷，
  症状不是报错，是「数据库突然空了」
- **同一个域名下 `/api` 走服务端、其余走 Web**，靠边缘的 PathPrefix + StripPrefix，
  优先级显式写死
- **`CORMEX_VERSION` 与 Cormex 的 `CORMEX_VERSION` 是两个变量**，独立升级

**没有 CI 部署。** 部署是手工 ssh + `docker compose up -d`。