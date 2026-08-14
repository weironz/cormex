-- 附件的原始文件名
--
-- 【为什么归属于引用（episode_blobs）而不是内容（blobs）】
--
-- blobs 是内容寻址的：hash 由字节决定，同一份内容只有一行。而**同一份内容
-- 完全可以有多个文件名** —— 同一张图今天叫 `设计稿.png`、下周被人转发过来
-- 叫 `IMG_2043.png`，两次上传落到同一个 hash 上。把 filename 放进 blobs
-- 就等于「谁先传谁定名」，后来者看到的是别人给的名字；改成后写覆盖又违反
-- append-only，还会让别的会话里那条消息的文件名凭空变掉。
--
-- 文件名是**这一次引用**的属性，不是内容的属性，所以它属于 episode_blobs。
-- mime 与 size_bytes 反过来：它们由字节唯一决定，本来就在 blobs 里，
-- 读的时候 JOIN 出来即可，不该在这里再存一份（存两份就会不一致）。
--
-- 【为什么可为空】
--
-- 老数据没有文件名，回填不出来（原始文件名从来没被记录过）。客户端遇到
-- NULL 应回落到「文档 · <hash 前 8 位>」那套显示，与本次改动之前一致。
ALTER TABLE episode_blobs
    ADD COLUMN filename TEXT;

-- 上限是防御性的：文件名来自客户端，没有上限时一个 10 MB 的「文件名」
-- 会进 sync_log 下发给所有设备。255 与主流文件系统的单段上限一致。
ALTER TABLE episode_blobs
    ADD CONSTRAINT episode_blobs_filename_len
    CHECK (filename IS NULL OR (length(filename) BETWEEN 1 AND 255));

COMMENT ON COLUMN episode_blobs.filename IS
    '这次引用时的原始文件名。内容寻址下同一份 blob 可以有多个文件名，
     因此它属于引用而非内容。NULL = 未知（老数据，或客户端没提供）。';
