# 采集幂等：同一段对话只录一次

> 状态：已实现（`3d3205e`，2026-11-07）。决定编号 D24，见
> [session-archive-architecture.md](session-archive-architecture.md) §1、§5.3、§7.1。
> 本文把散在架构文档里的规则收拢成一处，回答一个问题：**无论发生什么，同一段原始会话
> 会不会被录进 Cormex 两次。**

## 0. 一句话

**服务端存的字节是唯一的事实来源。** 客户端只要对一个文件「没有记忆」，就不信偏移、不信路径、
不信设备 id，而是拿本地文件前 n 字节的 sha256 请服务端核对，再决定接着追加、什么都不传、
开新一代，还是跟随同一会话里已经归档的那一份。

## 1. 问题从哪来

### 1.1 三层身份

| 身份 | 键 | 回答的问题 |
|---|---|---|
| 会话 `capture_sessions` | `(agent, native_session_id)` | 这是哪一段对话 |
| 文件 `capture_files` | `(device_id, agent, logical_path)` | 这些字节**在哪里**被看见 |
| 代 `capture_generations` | `(file_id, ordinal)` | 这个文件的一个连续字节流；追加不开新代，改写 / 截断开新代 |

文件身份是「在哪里看见的」的正确键，却是「这些字节是什么」的错误键。下面这些普通事件都会给
同一段对话造出第二个文件身份：

- 卸载后重装，没恢复旧设备密钥 → 新的 `device_id`
- 宿主挪了文件：Codex 把 rollout 移进 `archived_sessions/`，Claude Code 项目目录改名，
  被挪到一旁的 `.orphaned-…` 副本
- 同一个会话出现在第二台机器上

### 1.2 修复之前的两个缺陷

1. **按文件身份去重**：上面每种情况都把整段历史重新上传一遍；投影器的事件 id 从代派生，
   于是界面上同一段对话出现两次。
2. **只信偏移**：`state.db` 丢了之后，客户端注册时服务端说「我到 12 345」，客户端就从
   12 345 接着传；上传时服务端回 409「我领先」，客户端就跳到服务端的末尾。偏移说明不了
   它之前的字节是什么 —— 卸载期间被改写的文件因此被**拼接**到旧历史后面，两段不相干的
   内容成了一条流，这比重复更糟。

## 2. 机制

### 2.1 前缀核验：`POST /capture/generations/{id}/verify`

```
请求  { "length": n, "sha256": "<本地前 n 个逻辑字节的 sha256>" }
响应  { "matches": true|false, "end_offset": <这一代当前末尾> }
```

- 服务端按块读出这一代前 n 个逻辑字节（解压、解密之后）流式计算哈希，比较。对**任意长度**
  都精确。没用块清单比较，是因为较短的本地副本通常停在服务端某个块的中间，那个块的哈希
  说明不了它的一部分。
- `n` 超出这一代的范围时直接 `matches=false`。
- **不是哈希预言机**：只有在这个会话里拥有文件的设备才能核验（否则 403）。别人无法拿它
  试探任意内容是否在你的档案里。
- 客户端一侧对 `.zst` 文件单次流式解压计算，不落盘。

### 2.2 三种结论（`crates/cormex/src/sync/reconcile.rs`）

设本地文件长 `local`，服务端这一代末尾 `end`，取 `n = min(local, end)` 做核验：

| 核验结果 | 关系 | 结论 | 动作 |
|---|---|---|---|
| 一致，且 `local ≥ end` | 服务端是本地的前缀 | `Extends{end}` | 从 `end` 接着传，只传新增部分 |
| 一致，且 `local < end` | 本地是更旧的副本 | `Behind` | 什么都不传 |
| 不一致 | 两份内容在较短的那份之内就分叉了 | `Diverged` | 开新一代（`rewrite` / `snapshot`），旧代原样保留 |

`end = 0`（空的代）算作 `Extends{0}`：空流是任何流的前缀。

### 2.3 跟随（adopt）：`POST /capture/files/{id}/adopt`

一个**新注册、还空着**的文件，注册响应里会带 `candidates`：同一会话、同一角色
（transcript / subagent / …）里其他文件**当前所在**的非空代，最长的在前，最多 8 个。

客户端逐个核验：

- `Extends` 或 `Behind` → 调 `adopt`，这个文件从此**跟随**那一代，之后从
  `min(本地长度, 那一代末尾)` 起只传新增部分
- `Diverged` → 试下一个；都不匹配就按全新文件从 0 上传

服务端对 `adopt` 的检查：调用方必须拥有这个文件；被跟随的代必须属于**同一会话、同一角色**
（否则 400）。跨会话的跟随会把两段对话拼成一段，所以从存储层就拒绝。

### 2.4 存储

```sql
capture_generation_adoptions (id, file_id, generation_id, verified_through, created_at)
-- append-only；ON DELETE CASCADE，purge 不受影响

capture_current_generations  -- 视图：每个文件当前所在的代
-- = 自己的代与跟随记录里 created_at 最新的那个
```

**所有读取「当前代」的地方都走这个视图**：注册、投影队列、`latest_generation`、写权限
检查。只认自己 `ORDER BY ordinal DESC` 的读者会悄悄忽略跟随关系，所以视图在 migration
里只定义一次。

追加一条自己的新代（比如后来被改写了）会自然取代更早的跟随；什么都不 update。

### 2.5 写权限

`PUT` 块到一代：设备拥有这一代所属的文件，**或者**它的某个文件当前跟随这一代。后半句让两台
机器上的同一会话追加到同一条流里，而不是各存一份。

### 2.6 投影只做一次

投影器按会话处理时记下已投影的代 id，同一代被多个文件跟随也只投影一次。事件 id 由
`(会话, 解析器版本, 代, 偏移)` 派生，同一代的同一字节区间永远是同一个 id。

## 3. 客户端的完整决策路径

```
扫描到一个文件
 ├─ state.db 里有它（正常路径，不访问网络就能判断）
 │   ├─ 快照型文件：整文件哈希没变 → 跳过；变了 → 新快照代
 │   ├─ 比已上传的短 → 新代（truncate）
 │   ├─ 已上传区间的尾部哈希变了 → 新代（rewrite）
 │   ├─ 一样长 → 跳过
 │   └─ 更长 → 追加 [已上传, 当前长度)
 │        └─ 上传时服务端回 409「我领先」→ 前缀核验 → Extends 跳过已有 / Behind 停 / Diverged 新代
 │
 └─ state.db 里没有它（首次见到、state.db 丢了、重装）→ 注册
     ├─ 服务端这个文件已有字节 → 前缀核验 → Extends 接着传 / Behind 不传 / Diverged 新代
     └─ 这个文件还空着 → 逐个核验 candidates → 匹配就 adopt，不匹配就从 0 上传
```

## 4. 场景对照

| 场景 | 修复前 | 修复后 | 测试 |
|---|---|---|---|
| 会话持续追加聊天 | 只传新增 | 只传新增 | `sync.rs` 既有用例 |
| 项目里新增会话 | 正常录入 | 正常录入 | 既有用例 |
| 本地 `state.db` 删了 | 不重复，但只信偏移 | 核验后一个字节都不重发 | `a_lost_state_db_sends_nothing_again` |
| 卸载重装，**新**设备密钥 | ❌ 整段历史重传，界面显示两遍 | ✅ 跟随已有的代 | `a_reinstall_with_a_new_device_key_follows_the_archived_copy` |
| 重装后又聊了几轮 | ❌ 全部重传 | ✅ 只传新的那几轮 | `a_reinstall_then_more_chat_sends_only_the_new_turn` |
| 宿主挪了文件 / 项目目录改名 | ❌ 新路径当新文件重传 | ✅ 跟随 | `a_file_the_host_moved_is_not_uploaded_again` |
| 第二台机器上有更旧的副本 | ❌ 重传 | ✅ 不传；以后只传多出的字节 | `an_older_copy_on_another_machine_sends_nothing_and_later_only_the_new_bytes` |
| 卸载期间文件被改写 | ❌ 拼接到旧历史后面 | ✅ 新一代，旧代保留 | `a_file_rewritten_while_uninstalled_starts_a_new_generation` |
| 同一会话 id 下其实是另一段内容 | — | ✅ 不合并 | `a_different_conversation_under_the_same_session_id_is_not_merged` |
| 会话在宿主里改了标题 | 不影响身份 | 不影响身份；标题取最新的元数据 | — |

上表中客户端用例在 `crates/cormex/tests/sync.rs`（其中 5 个在旧代码上失败）。另外：

- 存储层：`crates/cormex-store/tests/archive.rs` 的 `adoption_makes_two_files_one_stream`、
  `adoption_across_sessions_is_refused`、`a_new_own_generation_supersedes_an_adoption`
- HTTP：`crates/cormexd/src/capture.rs` 的
  `a_second_device_verifies_adopts_and_appends_to_one_stream`
- 投影：`crates/cormexd/src/archive_workers.rs` 的 `an_adopted_generation_is_projected_once`
- 端到端（真实服务端 + 真实 CLI）：`crates/e2e-capture/tests/roundtrip.rs` 的
  `a_reinstall_and_a_host_move_never_archive_the_history_twice`

## 5. 改名之后会话显示在哪个项目下

项目目录改名后，同一个会话会有两个文件：旧路径的（项目键是旧的）和新路径的（跟随旧文件的代）。
会话的项目取**最新出现的那个文件**的项目键（`first_seen_at` 最大），所以会话跟着新名字走。
宿主自己提供了项目元数据时，以元数据为准（包括显式的「不归入任何项目」）。

这条规则在项目列表的计数、展开后的会话列表、会话详情、索引器的排除检查四处一致
（`880275a`；测试 `a_session_follows_its_newest_file_into_a_renamed_project`）。四处不一致的
症状是「项目上写着 3 个聊天，展开却是空的」。

## 6. 不变量

- **什么都不删、不改。** 跟随是新增一行；分叉是开新一代，旧代原样可读。回放「当时归档里有
  什么」不受影响。
- **核验是精确的。** 只有字节逐个相同才算同一条流；没有「差不多」的启发式。
- **跨会话不合并。** 会话身份 `(agent, native_session_id)` 是合并的边界；存储层拒绝越界的跟随。
- **客户端可以随时失忆。** `state.db`、设备密钥、安装目录都可以丢，最坏代价是多几次核验
  请求，不是多一份数据。

## 7. 代价

- 每个「没有记忆」的文件多一次核验请求，客户端读一遍本地前缀、服务端读一遍对应的块。
  正常追加路径（有 `state.db`）不增加任何请求。
- 候选最多 8 个，按长度降序；同一会话同一角色超过 8 份不同副本的情况下，第 9 份以后的
  不会被尝试（会按新文件上传）。实际中一个会话的副本数是个位数。

## 8. 没解决的

- **修复之前已经重复的会话不会自动合并。** 新机制只保证从现在起不再产生重复。修复需要一次性
  扫描：同会话同角色里互为前缀的代，补跟随记录、重投影，同样只增不删。已排成单独任务。
- **内容相同、会话 id 不同的两段对话**（比如手工复制了一份会话文件并改了 id）按两段对话处理。
  这是有意的：会话 id 是宿主给的身份，Cormex 不越过它去猜。
