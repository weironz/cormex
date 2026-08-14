//! 内容寻址：哈希计算、校验与 storage key 派生。
//!
//! # 为什么是 SHA-256 而不是更快的 BLAKE3
//!
//! 不为速度，为**契约对齐**：migration 里的 `sha256` domain 是
//! `^[0-9a-f]{64}$`，S3 生态（`x-amz-checksum-sha256`）也原生支持它。
//! 换 BLAKE3 要同时改 schema 与外部接口，省下的那点 CPU 不值这个价。

use sha2::{Digest, Sha256};

use crate::error::{BlobError, Result};

/// 十六进制哈希的字符数。与 migration 的 `sha256` domain 一致。
pub const HASH_HEX_LEN: usize = 64;

/// 租户前缀之后、哈希分桶之前的那一段。留出命名空间，
/// 将来同一租户名下放别的东西（导出包、缩略图）不会和字节撞在一起。
const KEY_PREFIX: &str = "blobs";

/// 租户前缀的长度上限，取 Postgres 的 `NAMEDATALEN - 1`。
///
/// 不是随手定的余量：超过这个长度的名字**做不成 schema**（Postgres 会静默
/// 截断），于是两个「不同」的租户可能落在同一个 schema 上。既然它不可能对应
/// 一个真实租户，就不该被允许拼进 key。
const MAX_TENANT_LEN: usize = 63;

/// 计算内容哈希（小写十六进制）。
///
/// 这是内容寻址的**唯一**入口：同字节必得同 key，
/// 于是「上传」天然幂等、「去重」天然免费、对象天然不可变
/// —— 也正是 RustFS 不支持对象版本控制却无妨的原因：
/// 一个 key 的内容永远是同一份字节，「旧版本」这个概念根本不存在。
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 校验哈希形如 64 位小写十六进制。
///
/// # 这是安全边界，不是格式洁癖
///
/// 哈希是唯一由外部（客户端 URL 路径、同步队列、数据库行）流入存储路径的值。
/// [`LocalFsBlobStore`](crate::LocalFsBlobStore) 要拿它拼文件路径，
/// S3 要拿它拼 object key。放进来一个 `../../../etc/passwd`，前者就是任意文件读写。
///
/// 十六进制字符集里没有 `.`、`/`、`\`、`%`、NUL，所以**这一条检查
/// 就把全部路径遍历向量堵死了**——不需要在下游再做一次转义，
/// 也不该做：两处独立的检查必然有一天不一致。
///
/// # Errors
/// 不合形制时返回 [`BlobError::InvalidHash`]。
pub fn validate_hash(hash: &str) -> Result<()> {
    let well_formed = hash.len() == HASH_HEX_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(())
    } else {
        Err(BlobError::InvalidHash(hash.to_owned()))
    }
}

/// 一段已经过校验的租户前缀。
///
/// # 为什么是 newtype，而不是像哈希那样每次用之前再校验一遍
///
/// 哈希非那样不可：它**逐次**从外部流入（URL 路径、同步队列、数据库行），
/// 每一次都是新的一份不可信输入。租户前缀不是——它在建 store 时入场一次，
/// 之后每个 key 都复用同一份。若也走「每次再查一遍」，这道检查就会散布到
/// 两个后端的十来个方法里，而 [`validate_hash`] 的文档已经写明为什么那是错的：
/// 两处独立的检查必然有一天不一致。
///
/// 所以改成「构造即校验、类型即证明」：内部字段私有，唯一入口是
/// [`TenantPrefix::new`]，因此**拿到一个 `TenantPrefix` 就等于拿到了
/// 它已过校验的证明**，下游既不必再问，也无法绕过去伪造一个。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantPrefix(String);

impl TenantPrefix {
    /// 校验并构造。规则见 [`validate_tenant`]。
    ///
    /// # Errors
    /// 不合形制时返回 [`BlobError::InvalidTenant`]。
    pub fn new(prefix: impl Into<String>) -> Result<Self> {
        let prefix = prefix.into();
        validate_tenant(&prefix)?;
        Ok(Self(prefix))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TenantPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 校验租户前缀，只放行「Postgres 标识符」这一形状：
/// 首字符是 ASCII 字母或 `_`，其后为字母 / 数字 / `_`，
/// 长度不超过 [`MAX_TENANT_LEN`]。
///
/// # 这与哈希那道校验是同一条防线
///
/// 多租户下前缀是**第二个**直通存储路径的外部输入：它会成为
/// [`LocalFsBlobStore`](crate::LocalFsBlobStore) 文件路径的一段，也会成为
/// S3 object key 的首段。放进来一个 `../../..`，后果与放进来一个坏哈希完全相同。
///
/// 因此这里沿用同一种做法：**靠字符集放行，而不是靠黑名单拦截**。
/// 允许的字符里没有 `.`、`/`、`\`、`%`、NUL、空白，于是 `..`、`a/../b`、
/// `..%2f..`、`\..\windows` 这些载荷一条都不必单独列举就已被拒——
/// 黑名单永远少写一条，白名单不会。
///
/// # 大小写：这里**两种都收**，但上游只铸小写
///
/// `cortex_store::SchemaName` 现在只产出 `public` 与 `u_` 加 **26 位小写**
/// ULID。小写是被一次实测逼出来的：`search_path` 里未加引号的标识符会被
/// Postgres 折成小写，而 schema 是用 `CREATE SCHEMA "u_ABC"` 建的、保留大写 ——
/// 两者对不上时 `search_path` 指向一个**不存在**的 schema，而 Postgres
/// 对此是**静默跳过**的，于是所有写入落进 `public`（也就是 1 号用户的库），
/// 且 `migrate()` 返回 `Ok`。
///
/// 那这里为什么还收大写？因为**这一层不该替上游做规范化**。它的职责是
/// 「拒掉能穿越目录、能变成两个 key 的东西」，而大写不在其中。
///
/// 残留的隐患是真的：NTFS / APFS 大小写不敏感，`Ab/` 与 `ab/` 是同一个目录，
/// 而 S3 的 key 大小写敏感，是两个对象——同一份代码在两个后端上行为不同。
/// 它在这里不咬人，只因为上游铸出来的形状永远不会「仅大小写不同」。
///
/// **由此推出一条禁令**：绝不要在这里加一步「统一转小写」的归一化。那会让
/// 任何已经按大写存下的对象在一次升级之后集体查无此 key——而且不报错，
/// 只是「历史附件全没了」。
///
/// # 另外三条规则，各自换掉一类静默故障
///
/// - **拒 `-` 与 `$`**。带连字符的 schema 名在 SQL 里处处都得加双引号，而
///   store 层有约 77 条查询。挡在这里，比指望每一条都记得加引号可靠得多。
/// - **拒数字开头**。Postgres 未加引号的标识符不许，同样是「不加引号就用不了」。
/// - **拒空串**。空前缀会让 key 退化成 `/blobs/…`（首段为空），本地后端上
///   `root.join("/blobs/…")` 在 Unix 上会被当成绝对路径而**丢弃 root**——
///   这是 `Path::join` 一个真实存在、且不报错的行为。
///
/// # Errors
/// 不合形制时返回 [`BlobError::InvalidTenant`]。
fn validate_tenant(prefix: &str) -> Result<()> {
    let mut bytes = prefix.bytes();
    let head_ok = matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_');
    let tail_ok = bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');

    if head_ok && tail_ok && prefix.len() <= MAX_TENANT_LEN {
        Ok(())
    } else {
        Err(BlobError::InvalidTenant(prefix.to_owned()))
    }
}

/// 由租户前缀与哈希派生对象 key：`<tenant>/blobs/ab/cd/<64 位哈希>`。
///
/// 两级哈希分桶不是为了好看：
/// - `LocalFsBlobStore` 若把百万个文件平铺进一个目录，ext4 的 htree
///   与 NTFS 的目录索引都会明显退化，`ls` 更是直接卡死
/// - S3 侧前缀影响分区，而哈希前缀天然均匀
///
/// 两个后端**必须共用这个函数**，key 才在后端之间可移植——
/// 从 LocalFs 迁到 RustFS 时 `blobs.storage_key` 那一列不用重写。
///
/// # 为什么租户在最外层，而 `blobs/` 留在它里面
///
/// 租户在最外层，「抹掉一个用户的全部字节」才是一次前缀删除
/// （`aws s3 rm --recursive s3://bucket/<tenant>/`、本地 `rm -rf root/<tenant>`），
/// 按用户统计用量也才是一次前缀聚合。掉个个儿写成 `blobs/<tenant>/…`，
/// 这两件事都要变成全桶扫描。
///
/// `blobs/` 不去掉：它原本的作用（同一个桶里放别的东西不会撞）现在退到租户
/// 内部继续生效——将来的 `<tenant>/exports/` 之类不该和字节混在一层。
///
/// # 代价是跨租户不再去重，这是买来的而不是漏掉的
///
/// 同一份字节被两个租户引用会存两份。换回来的是 GC 重新变成 schema 内部的
/// 问题：A 删掉最后一条引用时不必去问「B 还引用着吗」——而在 schema 隔离下
/// 那个问题根本无从查起，A 的连接池的 `search_path` 里看不见 B 的表。
/// 多占的磁盘有界且可估，跨 schema 的引用计数无解。
///
/// # Errors
/// 哈希非法时返回 [`BlobError::InvalidHash`]。
pub fn storage_key(tenant: &TenantPrefix, hash: &str) -> Result<String> {
    validate_hash(hash)?;
    Ok(format!(
        "{tenant}/{KEY_PREFIX}/{}/{}/{hash}",
        &hash[0..2],
        &hash[2..4]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(s: &str) -> TenantPrefix {
        TenantPrefix::new(s).unwrap_or_else(|e| panic!("{s:?} 本该是合法租户前缀，却被拒了：{e}"))
    }

    #[test]
    fn same_bytes_yield_same_key() {
        let a = sha256_hex(b"cortex");
        assert_eq!(
            a,
            sha256_hex(b"cortex"),
            "内容寻址的根基：同字节必须得到同一个 key，否则去重与幂等全部失效"
        );
        assert_ne!(
            a,
            sha256_hex(b"cortey"),
            "一字之差就该是完全不同的 key，否则去重会张冠李戴"
        );
    }

    #[test]
    fn hash_matches_migration_domain() {
        let h = sha256_hex(b"");
        assert_eq!(
            h.len(),
            HASH_HEX_LEN,
            "长度须与 migration 的 sha256 domain 一致，否则入库时被 CHECK 拒绝"
        );
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "出现非小写十六进制字符，会被 Postgres 的 sha256 domain 拒绝：{h}"
        );
        // 空串的 SHA-256 是公认常量，用它兜住「算法确实是 SHA-256」这条
        assert_eq!(
            h, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "空输入的摘要与 SHA-256 标准值不符，说明算法或十六进制编码搞错了"
        );
    }

    #[test]
    fn traversal_payloads_are_rejected() {
        // 每一条都可能真实出现在 URL 路径或离线同步队列里
        let payloads = [
            "../../../etc/passwd",
            "..",
            "a/../b",
            "..%2f..%2fetc",
            // 长度恰好 64 但塞了斜杠 —— 只查长度的实现会在这里失守
            "../../../../../../../../../../../../../../../../../etc/passwd/aaa",
            "\\..\\..\\windows\\system32",
            "0000000000000000000000000000000000000000000000000000000000000/..",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8\0\0",
        ];
        for h in payloads {
            assert!(
                validate_hash(h).is_err(),
                "路径遍历载荷未被拦截，LocalFsBlobStore 将出现任意文件读写：{h:?}"
            );
            assert!(
                storage_key(&tenant("public"), h).is_err(),
                "storage_key 未复用校验，非法哈希漏进了对象 key：{h:?}"
            );
        }
    }

    #[test]
    fn malformed_hashes_are_rejected() {
        let valid = sha256_hex(b"x");
        assert!(validate_hash(&valid).is_ok(), "合法哈希被误拒：{valid}");

        assert!(
            validate_hash(&valid.to_uppercase()).is_err(),
            "大写十六进制必须拒 —— 放行会让同一份内容产生两个 key，去重直接失效"
        );
        assert!(
            validate_hash(&valid[..HASH_HEX_LEN - 1]).is_err(),
            "截断的哈希必须拒，否则前缀碰撞会让不同内容映射到同一对象"
        );
        assert!(
            validate_hash(&format!("{valid}0")).is_err(),
            "超长的哈希必须拒"
        );
        assert!(validate_hash("").is_err(), "空哈希必须拒");
    }

    #[test]
    fn storage_key_layout_is_stable() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            storage_key(&tenant("public"), hash).unwrap(),
            format!("public/blobs/e3/b0/{hash}"),
            "key 布局是两个后端共享的契约，改动会让已入库的 storage_key 全部指错地方"
        );
    }

    #[test]
    fn tenant_is_the_outermost_segment() {
        // 「抹掉一个用户的全部字节」要能退化成一次前缀删除，租户就必须在首段。
        // 若哪天被改成 blobs/<tenant>/…，这条会红。
        let hash = sha256_hex(b"tenancy");
        let key = storage_key(&tenant("cortex_u7"), &hash).unwrap();
        assert!(
            key.starts_with("cortex_u7/"),
            "租户必须是 key 的首段，否则按用户删除与按用户统计都要全桶扫描，实际是 {key}"
        );
        assert_eq!(
            key,
            format!("cortex_u7/blobs/{}/{}/{hash}", &hash[0..2], &hash[2..4]),
            "租户前缀之后必须仍是 blobs/两级哈希分桶 —— 两个后端共享这套布局"
        );
    }

    #[test]
    fn different_tenants_never_share_an_object() {
        // 这正是这次改动买下的东西：放弃跨租户去重，换掉「A 删掉最后一条引用时
        // 字节还被 B 引用着」这一整类跨 schema 的 GC bug。
        let hash = sha256_hex(b"same bytes, two owners");
        let a = storage_key(&tenant("public"), &hash).unwrap();
        let b = storage_key(&tenant("cortex_u2"), &hash).unwrap();
        assert_ne!(
            a, b,
            "同一份字节在两个租户下必须落到不同 key —— 相同即意味着 GC 又变回跨 schema 问题"
        );
    }

    #[test]
    fn realistic_schema_names_are_accepted() {
        // 误拒的代价同样具体：这几个名字都已经在仓库里真实存在，
        // 拒掉任何一个都会让对应的部署或测试直接起不来。
        for ok in [
            "public",                       // 1 号用户的 schema 就叫这个（roadmap R9）
            "u_01bx5zzkbkactav9wevgemmvrz", // cortex_store::SchemaName 现在铸的形状（小写）
            "u_01BX5ZZKBKACTAV9WEVGEMMVRZ", // 大写也收 —— 这一层不替上游做规范化
            "cortex_auth",                  // 认证表所在的 schema
            "cortex_test_1_2_3",            // cortex-store 测试 harness 生成的临时 schema
            "_leading_underscore",          // Postgres 允许下划线开头
            "t",                            // 单字符
            "u2026",                        // 数字结尾
            &"a".repeat(MAX_TENANT_LEN),    // 恰好 63，边界之内
        ] {
            assert!(
                TenantPrefix::new(ok).is_ok(),
                "{ok:?} 是合法的 Postgres schema 名，误拒会让对应租户完全无法读写对象"
            );
        }
    }

    /// 把「哪些形态漏过去了」一次报全，而不是撞上第一条就 panic。
    ///
    /// 逐条 `assert!` 在这里是有害的：校验一旦被改坏，测试只会报出**第一个**
    /// 漏网的载荷，修完再跑又报下一个 —— 而真正该看到的是「一共漏了哪几类」。
    fn assert_all_rejected(payloads: &[(&str, &str)]) {
        let escaped: Vec<String> = payloads
            .iter()
            .filter(|(evil, _)| TenantPrefix::new(*evil).is_ok())
            .map(|(evil, why)| format!("  {evil:?} —— {why}"))
            .collect();
        assert!(
            escaped.is_empty(),
            "以下 {} 种前缀本该被拒却放行了：\n{}",
            escaped.len(),
            escaped.join("\n")
        );
    }

    #[test]
    fn traversal_prefixes_are_rejected() {
        // 每一条都对应「前缀逃出 root」的一种走法。校验一旦失守，
        // LocalFsBlobStore 就是任意路径读写 —— 与坏哈希的后果完全相同。
        assert_all_rejected(&[
            ("..", "上一级目录"),
            (".", "当前目录：会让两个租户的 key 归一"),
            ("../..", "多级上跳"),
            ("../etc", "上跳后指向系统目录"),
            ("a/b", "内嵌斜杠：凭空多切一层前缀"),
            ("/abs", "绝对路径：Unix 上 root.join 会直接丢掉 root"),
            ("acme/", "尾随斜杠"),
            ("..\\..", "Windows 分隔符"),
            ("a\\b", "内嵌反斜杠"),
            ("..%2f..", "URL 编码的斜杠：只拦字面 / 的实现在这里失守"),
            ("a\0b", "嵌入 NUL：C 层截断后剩下的路径与看到的不是同一个"),
        ]);
    }

    #[test]
    fn empty_prefix_is_rejected() {
        assert!(
            TenantPrefix::new("").is_err(),
            "空前缀必须拒：key 会退化成 /blobs/… 首段为空，本地后端上 root 会被 join 丢掉"
        );
    }

    /// 大写必须**放行**，且这条要钉死：`cortex_store::SchemaName` 铸出来的
    /// 用户 schema 是 `u_` 加 26 位大写 ULID。若哪天有人「顺手收严成只收小写」，
    /// 后果是除 1 号用户（`public`）外没有任何租户能建出 store。
    ///
    /// 同理也钉死「不做转小写归一化」：那会让已按大写 ULID 存下的对象
    /// 在升级后集体查无此 key —— 不报错，只是历史附件全不见了。
    #[test]
    fn uppercase_ulid_schemas_are_accepted_verbatim() {
        let ulid = "u_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let t = TenantPrefix::new(ulid)
            .unwrap_or_else(|e| panic!("cortex-store 真实铸出的 schema 名被拒了：{e}"));
        assert_eq!(
            t.as_str(),
            ulid,
            "租户前缀必须原样保留大小写；做了转小写归一化，已入库的对象就再也取不回来"
        );
        assert!(
            storage_key(&t, &sha256_hex(b"x"))
                .unwrap()
                .starts_with(ulid),
            "大写 ULID 必须原样出现在 key 首段"
        );
    }

    /// 这些字符做 schema 名时在 SQL 里处处都得加双引号，
    /// 而 store 层有约 77 条查询 —— 漏一条就是运行期报错。挡在源头更可靠。
    #[test]
    fn punctuation_that_would_need_sql_quoting_is_rejected() {
        assert_all_rejected(&[
            ("acme-corp", "连字符"),
            ("acme$1", "美元符：Postgres 标识符允许，但 shell 会展开它"),
            ("acme.corp", "点号：同时也是路径遍历的原料"),
            ("acme corp", "空格"),
            ("1acme", "数字开头：Postgres 未加引号的标识符不许"),
        ]);
    }

    /// 非 ASCII 必须拒：文件系统的 Unicode 规范化各不相同（APFS 做 NFD，
    /// 多数 Linux 文件系统原样存），同一个名字在不同机器上可能落到不同目录，
    /// 而 S3 侧又是按字节比较的第三套行为。
    #[test]
    fn non_ascii_is_rejected() {
        assert_all_rejected(&[
            ("租户", "中文"),
            (
                "café",
                "带变音符号的拉丁字母：NFC/NFD 两种编码看起来一模一样",
            ),
        ]);
    }

    #[test]
    fn overlong_prefix_is_rejected() {
        let too_long = "a".repeat(MAX_TENANT_LEN + 1);
        assert!(
            TenantPrefix::new(&too_long).is_err(),
            "超过 {MAX_TENANT_LEN} 字节必须拒：Postgres 会静默截断 schema 名，\
             于是两个「不同」的租户可能落到同一个 schema 上"
        );
    }
}
