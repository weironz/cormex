//! 四路召回的冒烟测试。
//!
//! 只验证 SQL 语法正确、返回类型对得上 —— 排序质量属于检索评测的范畴
//! （见 docs/roadmap.md 的 P1「检索评测三件套」），不在这里断言。

mod common;

use chrono::Utc;

#[tokio::test]
async fn four_channels_execute_against_real_schema() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    // 空输入必须安全返回，不能panic也不能拼出非法 tsquery
    assert!(store.recall_bm25("", 10).await.unwrap().is_empty());
    assert!(store.recall_graph(&[], 2, 10).await.unwrap().is_empty());
    assert!(store.entities_matching(&[], 10).await.unwrap().is_empty());

    // 四路各自能跑通
    store
        .recall_bm25("对象 | 存储", 10)
        .await
        .expect("BM25 召回");
    let v = cortex_store::Vector::from(vec![0.1f32; cortex_store::EMBEDDING_DIM]);
    store
        .recall_vector(&v, "bge-m3", 10)
        .await
        .expect("向量召回");
    store.recall_recent(7, 10).await.expect("近因召回");
    store
        .recall_graph(&["01JZZZZZZZZZZZZZZZZZZZZZZ1".into()], 2, 10)
        .await
        .expect("图遍历召回");
    store.facts_as_of(Utc::now(), 10).await.expect("时间回放");

    // limit 越界必须被夹住而不是让数据库报错
    store
        .recall_recent(99999, 99999)
        .await
        .expect("limit 应被 clamp");
}
