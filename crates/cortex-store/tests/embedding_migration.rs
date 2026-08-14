//! No silent amnesia when switching embedding models — the storage layer's half.
//!
//! # What is guarded here
//!
//! All four vector queries filter by `embedding_model`, so "computing distances
//! across models and getting garbage" cannot happen. The price is that on the
//! day of the switch **the vector channel returns 0 rows** — while BM25, graph
//! traversal, and recency carry on working, so it does not look broken, only
//! like "semantic retrieval seems a bit worse". Nobody is told.
//!
//! The first test below **reproduces that failure first** and only then verifies
//! that the side table restores it. A test that only verified the latter would
//! be meaningless: it could not prove the path was broken to begin with.

mod common;

use cortex_store::{EMBEDDING_DIM, NewFactEmbedding, Vector};

/// Build a unit vector with a definite direction: dimension `i` is 1.
///
/// Not a random vector: with randomness, "what is the k-th nearest" comes down
/// to luck and the assertion could only say "some rows came back" — and that
/// kind of assertion is blind to ordering errors.
fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[i % EMBEDDING_DIM] = 1.0;
    v
}

/// Write n facts carrying vectors, all recorded under `model`. Returns their
/// ids.
async fn seed(ctx: &common::TestDb, model: &str, n: usize) -> Vec<String> {
    let ep = common::new_episode("s-embed", "换模型测试");
    let ent = common::new_entity("project", "cortex");
    let facts: Vec<_> = (0..n)
        .map(|i| {
            let mut f = common::new_fact(ent.id, "uses", &format!("thing-{i}"), ep.id);
            f.embedding = Some(Vector::from(unit(i)));
            f.embedding_model = model.to_owned();
            f
        })
        .collect();
    let ids: Vec<String> = facts.iter().map(|f| f.id.to_string()).collect();

    ctx.store
        .write_txn(async |tx| {
            tx.insert_episode(&ep).await?;
            tx.insert_entity(&ent).await?;
            for f in &facts {
                tx.insert_fact(f).await?;
            }
            Ok(())
        })
        .await
        .expect("writing the seed data");
    ids
}

#[tokio::test]
async fn switching_model_silently_drops_vector_recall_until_backfilled() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    let ids = seed(&ctx, "old-model", 5).await;
    let probe = Vector::from(unit(0));

    // ── ① Everything is fine under the old model ───────────
    let hit = store
        .recall_vector(&probe, "old-model", 10)
        .await
        .expect("vector retrieval");
    assert_eq!(
        hit.len(),
        5,
        "all 5 rows should be retrieved under the old model"
    );
    assert_eq!(
        hit[0].id, ids[0],
        "a probe with dimension 0 set to 1 should have row 0 as its nearest -- a wrong order \
         means the outer level is not sorting by distance"
    );

    // ── ② Switch models: **this is the silent failure** ────
    let after = store
        .recall_vector(&probe, "new-model", 10)
        .await
        .expect("vector retrieval");
    assert!(
        after.is_empty(),
        "after a model switch the old vectors must return nothing at all (distances across \
         spaces are not comparable). The real problem is that this **raises no error** -- BM25 \
         and graph traversal keep covering for it, and in the UI it shows up only as \"semantic \
         retrieval seems a bit worse\". It returned {} rows",
        after.len()
    );

    // ── ③ Backfill: insert new-space vectors into the side table ──
    store
        .write_txn(async |tx| {
            for (i, id) in ids.iter().enumerate() {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("the fact id is a ULID"),
                    embedding_model: "new-model".to_owned(),
                    // Deliberately the **reversed** direction: if the union
                    // query wrongly picked up the vector from the original
                    // column, the ordering would come out opposite to what is
                    // expected here and the test would catch it
                    embedding: Vector::from(unit(4 - i)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("backfilling the side-table vectors");

    // ── ④ Restored ─────────────────────────────────────────
    let healed = store
        .recall_vector(&probe, "new-model", 10)
        .await
        .expect("vector retrieval");
    assert_eq!(
        healed.len(),
        5,
        "after the backfill all 5 rows should be retrievable again under the new model"
    );
    assert_eq!(
        healed[0].id, ids[4],
        "in the side table it is row 4 that has dimension 0 set to 1 -- getting ids[0] means the \
         union used the vector from the original column rather than the side table, which makes \
         the backfill worthless"
    );

    // ── ⑤ The old model is unaffected ──────────────────────
    let old_still = store
        .recall_vector(&probe, "old-model", 10)
        .await
        .expect("vector retrieval");
    assert_eq!(
        old_still.len(),
        5,
        "the backfill must not touch the old space -- dual-model retrieval (losing no memory \
         during a backfill) rests entirely on this"
    );

    ctx.cleanup().await;
}

#[tokio::test]
async fn a_fact_present_on_both_sides_is_returned_once() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    // The original column is already same-model, and we stuff another row for
    // the **same model** into the side table -- a normal backfill cannot
    // manufacture this overlap (the backfiller only picks facts whose
    // embedding_model <> the current one), but "switch away and switch back"
    // can. The symptom of a duplicate is the same memory appearing twice in
    // retrieval with RRF giving it a double ranking: a quietly amplifying bias
    // rather than an error
    let ids = seed(&ctx, "same-model", 3).await;
    store
        .write_txn(async |tx| {
            tx.insert_fact_embedding(&NewFactEmbedding {
                fact_id: ids[0].parse().expect("the fact id"),
                embedding_model: "same-model".to_owned(),
                embedding: Vector::from(unit(7)),
            })
            .await
        })
        .await
        .expect("writing the side-table vector");

    let hit = store
        .recall_vector(&Vector::from(unit(0)), "same-model", 10)
        .await
        .expect("vector retrieval");
    assert_eq!(
        hit.len(),
        3,
        "a fact hit on both sides must appear only once; got {hit:?}"
    );

    let mut seen: Vec<&str> = hit.iter().map(|f| f.id.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        3,
        "the count should not change after deduplication -- if it does, there were duplicate rows"
    );

    ctx.cleanup().await;
}

#[tokio::test]
async fn census_and_backlog_agree_with_each_other() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };
    let store = &ctx.store;

    let ids = seed(&ctx, "old-model", 4).await;

    // Census: only the old model
    let census = store.embedding_census().await.expect("the census");
    assert_eq!(
        census,
        vec![("old-model".to_string(), 4)],
        "the census should show only the old model tier, covering 4 rows"
    );

    // Backlog: all 4 lack a vector for the new model
    let todo = store
        .facts_missing_embedding("new-model", 100)
        .await
        .expect("fetching the backlog");
    assert_eq!(
        todo.len(),
        4,
        "on a model switch all 4 rows should be in the backlog"
    );

    // Backfill two of them
    store
        .write_txn(async |tx| {
            for id in ids.iter().take(2) {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("the fact id"),
                    embedding_model: "new-model".to_owned(),
                    embedding: Vector::from(unit(1)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("backfilling two rows");

    // Census: both tiers coexist, which is exactly what "a backfill in
    // progress" should look like
    let census = store.embedding_census().await.expect("the census");
    assert_eq!(
        census,
        vec![("old-model".to_string(), 4), ("new-model".to_string(), 2)],
        "halfway through the backfill both tiers are present, ordered by coverage descending"
    );

    // The backlog is two shorter -- this assertion also guards "the backfiller
    // does not redo the same row over and over"
    let todo = store
        .facts_missing_embedding("new-model", 100)
        .await
        .expect("fetching the backlog");
    assert_eq!(
        todo.len(),
        2,
        "the two already backfilled must disappear from the backlog, or the backfiller loops \
         forever redoing the same batch"
    );

    ctx.cleanup().await;
}

/// Query-plan guard — the base set **must not be materialized**.
///
/// # What it verifies, and what it does not
///
/// **It verifies**: that `CTE Scan` does not appear in the plan. That is a
/// property of the plan's **shape**, independent of data volume, so it is
/// accurate even at test scale.
///
/// **It does not verify**: whether either side genuinely uses HNSW. That depends
/// on row counts and statistics — the test database holds a handful of rows, at
/// which scale the planner picks a btree (`idx_facts_model`) rather than HNSW,
/// and not even `enable_seqscan = off` can force it (that only turns off
/// sequential scans; the btree remains). Verifying index selection would require
/// manufacturing thousands of rows × 1024 dimensions: slow, brittle, and
/// verifying the planner's cost model rather than how we wrote the query.
///
/// # Why this half is still worth writing
///
/// The first version factored the base set into `WITH base AS (…)`. Each of the
/// two branches references it once, and since PG 12 a CTE is inlined only when
/// referenced **once**, so it got materialized — and branch one's
/// `ORDER BY embedding <=> $1` could only sort a pile of already-materialized
/// rows, putting HNSW completely out of play.
///
/// That formulation runs blazingly fast on a small database and only surfaces
/// once the database grows, at which point it looks like "the database got
/// slower" rather than "the index isn't being used". This test stops exactly
/// that.
///
/// # And a note on a problem that **predates this change**
///
/// Branch one in fact **could not use** `idx_facts_vec`, for reasons unrelated
/// to this change: `active_facts` was a view with a LEFT JOIN (joining
/// `fact_status`, itself a DISTINCT ON over `fact_events`), and the planner had
/// to finish the join before it could sort by distance. The plan for that query
/// before the change was:
///
/// ```text
///   Sort (Sort Key: f.embedding <=> …)
///     ->  Nested Loop Left Join
///           ->  Seq Scan on facts f
///           ->  Sort  ->  Seq Scan on fact_events
/// ```
///
/// In other words, the HNSW on the main retrieval path had been decorative all
/// along. **Fixed**: migrations/20260810000001 replaced the view with a
/// two-level anti-join, taking 289.7ms → 0.5ms over 50,000 facts. The "no outer
/// join allowed" assertion below is what guards that.
#[tokio::test]
async fn the_base_set_is_not_materialized() {
    let Some(ctx) = common::setup().await else {
        eprintln!("跳过：数据库不可用");
        return;
    };

    let ids = seed(&ctx, "m", 3).await;
    ctx.store
        .write_txn(async |tx| {
            for id in &ids {
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id: id.parse().expect("the fact id"),
                    embedding_model: "other".to_owned(),
                    embedding: Vector::from(unit(2)),
                })
                .await?;
            }
            Ok(())
        })
        .await
        .expect("the side table needs rows too, or branch two never appears in the plan at all");

    // `query_raw` does not support bind parameters, so the three parameters are
    // spliced in as literals. A 1024-dimensional vector literal runs to several
    // KB -- acceptable in a test, not in production code
    let probe = format!(
        "'[{}]'::vector",
        unit(0)
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let sql = cortex_store::RECALL_VECTOR_SQL
        .replace("$1", &probe)
        .replace("$2", "'m'")
        .replace("$3", "10");

    let text = ctx.query_raw(format!("EXPLAIN {sql}")).await.join(
        "
",
    );

    assert!(
        !text.contains("CTE Scan"),
        "a CTE Scan appeared in the plan -- the base set was materialized and the vector index \
         is completely out of play. Do not factor base into a `WITH`: each of the two branches \
         references it once, so PG will not inline it."
    );
    // Both branches must genuinely appear in the plan -- one missing means the
    // union formulation optimized one side away, and that is exactly what
    // "backfilled but not retrievable" looks like
    assert!(
        text.contains("facts"),
        "the plan contains no facts -- branch one is gone:
{text}"
    );
    assert!(
        text.contains("fact_embeddings"),
        "the plan contains no fact_embeddings -- branch two is gone, making the backfill \
         worthless:
{text}"
    );

    // **No outer join may precede the ORDER BY.**
    //
    // This guards the shape of active_facts. It used to be
    // `facts LEFT JOIN fact_status`, and an outer join must be **finished
    // first** before anything can be sorted by distance -- so idx_facts_vec was
    // never used and the HNSW on the main retrieval path was decorative all
    // along. An anti-join (NOT EXISTS) is different: the planner can treat it
    // as a filter condition on facts, which puts the index within reach.
    //
    // # Why not simply assert "idx_facts_vec appears in the plan"
    //
    // Tried it; it cannot be tested. The test database holds single digits of
    // rows, and at that scale a sequential scan or a small btree is always
    // cheaper for the planner -- not even `enable_seqscan = off` can force HNSW
    // out of it; it switches to idx_facts_model and sorts afterwards. **On 4
    // rows of data, "what the planner chose" proves nothing**, so what is
    // asserted here is structure rather than choice.
    //
    // The quantitative evidence is in migrations/20260810000001: over 50,000
    // facts, 289.7ms → 0.5ms, and 198,000 buffer accesses → 531.
    assert!(
        !text.contains("Left Join"),
        "an outer join appeared in the plan -- active_facts has gone back to the shape that \
         \"must finish the join first\".
This does not make the results wrong, it just degrades every vector retrieval into a full table
scan plus tens of thousands of 1024-dimensional distance computations, and no other test will go
red for it. See migrations/20260810000001:
{text}"
    );

    ctx.cleanup().await;
}
