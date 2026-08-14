-- 工具调用记下「这次写入改了什么」。
--
-- 为什么要落库：不落的话，重开会话就只剩一行 `write_file  pubspec.yaml`，
-- 而工具行恰恰是事后会回来看的那个东西。这与 R1-R5 那一批「回放保真度」
-- 是同一条主张 —— 界面上当时能看见的，回放时也要能看见。
--
-- 只有 write_file 会填它：只有那个工具手上同时有旧内容与新内容。
-- shell 跑完文件变成什么样，agent 并不知道。
ALTER TABLE episode_tool_calls ADD COLUMN diff TEXT;

-- 8 KiB 上限，与 summary 那条 2048 是同一类防御，只是量级不同。
--
-- 这一列会随 `sync_log` **下发到所有设备**：一次改动几百行的重构，
-- 完整 diff 轻松几十 KB，乘以设备数、乘以每轮多个 write_file，
-- 就是同步通道上一笔谁都没预算过的流量。
--
-- 真正的截断在 agent 侧就做完了（`cortex_agent::diff` 的行数 + 字符数
-- 双上限），那里还能在末尾写明「其余未显示」。数据库这条是最后一道栅栏：
-- 它挡的不是正常路径，是「哪天有人绕过 agent 直接塞一条进来」。
ALTER TABLE episode_tool_calls
    ADD CONSTRAINT episode_tool_calls_diff_len
    CHECK (diff IS NULL OR length(diff) <= 8192);

COMMENT ON COLUMN episode_tool_calls.diff IS
    '这次写入的统一 diff（已截断，见 cortex_agent::diff）。
     NULL = 这个工具没有可看的改动：不碰文件的工具、内容一字未变的写入、
     以及写失败的那次。';
