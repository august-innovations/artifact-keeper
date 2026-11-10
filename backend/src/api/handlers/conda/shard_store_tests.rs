//! Tests of the stored shards and revisions of hosted conda channels
//! (#4173, #4174).

use super::revision_router_tests::{get, upload};
use super::shard_store;
use crate::api::handlers::test_db_helpers as tdh;
use axum::http::StatusCode;
use std::collections::BTreeMap;
use std::io::Read;

/// `(package_name, shard_hash, updated_at)` of every stored shard row.
async fn index_rows(
    fx: &tdh::Fixture,
    subdir: &str,
) -> BTreeMap<String, (Option<Vec<u8>>, chrono::DateTime<chrono::Utc>)> {
    let rows: Vec<(String, Option<Vec<u8>>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT package_name, shard_hash, updated_at FROM conda_shard_index \
          WHERE repository_id = $1 AND subdir = $2",
    )
    .bind(fx.repo_id)
    .bind(subdir)
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    rows.into_iter().map(|(n, h, u)| (n, (h, u))).collect()
}

async fn dirty_count(fx: &tdh::Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM conda_shard_dirty WHERE repository_id = $1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap()
}

/// The shard index's `name -> address` map, decoded.
fn decode_index(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let index: rattler_conda_types::ShardedRepodata =
        rmp_serde::from_slice(&zstd::decode_all(body).unwrap()).unwrap();
    index
        .shards
        .into_iter()
        .map(|(n, h)| (n, h.to_vec()))
        .collect()
}

/// The filenames a decoded shard lists.
fn shard_filenames(body: &[u8]) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Shard {
        packages: BTreeMap<String, serde::de::IgnoredAny>,
        #[serde(rename = "packages.conda")]
        packages_conda: BTreeMap<String, serde::de::IgnoredAny>,
    }
    let shard: Shard = rmp_serde::from_slice(&zstd::decode_all(body).unwrap()).unwrap();
    let mut out: Vec<String> = shard
        .packages
        .into_keys()
        .chain(shard.packages_conda.into_keys())
        .collect();
    out.sort();
    out
}

async fn shard_of(fx: &tdh::Fixture, subdir: &str, name: &str) -> Option<Vec<String>> {
    let (status, body, _) = get(fx, &format!("{subdir}/repodata_shards.msgpack.zst")).await;
    assert_eq!(status, StatusCode::OK);
    let address = decode_index(&body).remove(name)?;
    let (status, shard, _) = get(
        fx,
        &format!("{subdir}/shards/{}.msgpack.zst", hex::encode(address)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    Some(shard_filenames(&shard))
}

/// #4173 acceptance: one upload rewrites exactly one shard row (the
/// uploaded name's), and it is built at write time, nothing left dirty.
#[tokio::test]
async fn an_upload_rewrites_exactly_one_shard_row_4173() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    upload(&fx, "noarch", "one-a", "1.0").await;
    upload(&fx, "noarch", "one-b", "1.0").await;
    assert_eq!(dirty_count(&fx).await, 0, "built at write time");
    let before = index_rows(&fx, "noarch").await;
    assert_eq!(before.len(), 2);

    let added = upload(&fx, "noarch", "one-a", "2.0").await;
    assert_eq!(dirty_count(&fx).await, 0);
    let after = index_rows(&fx, "noarch").await;
    assert_eq!(after.len(), 2);
    assert_ne!(
        after["one-a"], before["one-a"],
        "one-a's shard is rewritten"
    );
    assert_eq!(after["one-b"], before["one-b"], "one-b's row is untouched");

    let listed = shard_of(&fx, "noarch", "one-a").await.unwrap();
    assert!(listed.contains(&added), "{listed:?}");
    assert_eq!(listed.len(), 2);
}

/// #4174 acceptance: a withdrawal is absent from every encoding after the
/// next read, and from the package's shard. The withdrawal here is a plain
/// catalog update (as the quarantine console or a scanner verdict makes it),
/// not a conda write path: the trigger marks the key dirty in the update's
/// own transaction and the read rebuilds it before serving.
#[tokio::test]
async fn a_withdrawal_leaves_every_encoding_and_the_shard_4174() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let kept = upload(&fx, "linux-64", "wd-a", "1.0").await;
    let pulled = upload(&fx, "linux-64", "wd-a", "2.0").await;
    upload(&fx, "linux-64", "wd-b", "1.0").await;

    // Warm every document of the subdir at the pre-withdrawal revision.
    for doc in [
        "repodata.json",
        "repodata.json.zst",
        "repodata.json.bz2",
        "current_repodata.json",
        "run_exports.json",
        "repodata.json.jlap",
        "repodata_shards.msgpack.zst",
    ] {
        let (status, body, _) = get(&fx, &format!("linux-64/{doc}")).await;
        assert_eq!(status, StatusCode::OK, "{doc}");
        if doc == "repodata.json" {
            assert!(String::from_utf8_lossy(&body).contains(&pulled));
        }
    }

    sqlx::query(
        "UPDATE artifacts SET quarantine_status = 'rejected' \
          WHERE repository_id = $1 AND path = $2",
    )
    .bind(fx.repo_id)
    .bind(format!("linux-64/{pulled}"))
    .execute(&fx.pool)
    .await
    .unwrap();
    assert!(
        dirty_count(&fx).await > 0,
        "the update marked the key dirty"
    );

    let packages_of = |doc: &serde_json::Value| -> Vec<String> {
        let mut out: Vec<String> = doc["packages"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        out.sort();
        out
    };
    let (_, json, _) = get(&fx, "linux-64/repodata.json").await;
    let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert!(!packages_of(&json).contains(&pulled));
    assert!(packages_of(&json).contains(&kept));
    assert!(json["removed"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(pulled)));

    let (_, zst, _) = get(&fx, "linux-64/repodata.json.zst").await;
    let zst: serde_json::Value =
        serde_json::from_slice(&zstd::decode_all(&zst[..]).unwrap()).unwrap();
    assert_eq!(packages_of(&zst), packages_of(&json));

    let (_, bz2, _) = get(&fx, "linux-64/repodata.json.bz2").await;
    let mut plain = Vec::new();
    bzip2::read::BzDecoder::new(&bz2[..])
        .read_to_end(&mut plain)
        .unwrap();
    let bz2: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(packages_of(&bz2), packages_of(&json));

    let (_, current, _) = get(&fx, "linux-64/current_repodata.json").await;
    let current: serde_json::Value = serde_json::from_slice(&current).unwrap();
    assert_eq!(packages_of(&current), vec![kept.clone(), filename_b()]);

    let (_, run_exports, _) = get(&fx, "linux-64/run_exports.json").await;
    let run_exports: serde_json::Value = serde_json::from_slice(&run_exports).unwrap();
    assert!(!packages_of(&run_exports).contains(&pulled));

    assert_eq!(
        shard_of(&fx, "linux-64", "wd-a").await.unwrap(),
        vec![kept.clone()]
    );
    assert_eq!(dirty_count(&fx).await, 0);

    // Withdrawing the last visible version keeps the name owned (its row
    // stays, without a shard) and drops it from the shard index.
    sqlx::query(
        "UPDATE artifacts SET quarantine_status = 'rejected' \
          WHERE repository_id = $1 AND path = $2",
    )
    .bind(fx.repo_id)
    .bind(format!("linux-64/{kept}"))
    .execute(&fx.pool)
    .await
    .unwrap();
    assert_eq!(shard_of(&fx, "linux-64", "wd-a").await, None);
    let rows = index_rows(&fx, "linux-64").await;
    assert_eq!(rows["wd-a"].0, None);

    fn filename_b() -> String {
        super::revision_router_tests::filename("wd-b", "1.0", "h0_0")
    }
}

/// A hold that lapses makes its package visible with no write at all
/// (#4059 semantics): the first read after the expiry rebuilds the key.
#[tokio::test]
async fn a_lapsed_hold_becomes_visible_without_a_write_4173() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let held = upload(&fx, "noarch", "hold-a", "1.0").await;
    sqlx::query(
        "UPDATE artifacts SET quarantine_status = 'quarantined', \
                quarantine_until = NOW() + INTERVAL '2 seconds' \
          WHERE repository_id = $1 AND path = $2",
    )
    .bind(fx.repo_id)
    .bind(format!("noarch/{held}"))
    .execute(&fx.pool)
    .await
    .unwrap();

    let (_, body, _) = get(&fx, "noarch/repodata.json").await;
    assert!(!String::from_utf8_lossy(&body).contains(&format!("\"{held}\": {{")));
    assert_eq!(shard_of(&fx, "noarch", "hold-a").await, None);

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let (_, body, _) = get(&fx, "noarch/repodata.json").await;
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        doc["packages"].get(&held).is_some(),
        "the lapsed hold is listed again: {doc}"
    );
    assert_eq!(
        shard_of(&fx, "noarch", "hold-a").await,
        Some(vec![held.clone()])
    );
}

/// #4173: a repository that held packages before the shard tables existed
/// is backfilled in bounded batches that resume from a saved cursor; until
/// it finishes its shard endpoints keep the per-request walk, and the stored
/// index it ends with is byte-identical to the walk's.
#[tokio::test]
async fn backfill_fills_a_preexisting_channel_in_resumable_batches_4173() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    for name in ["bf-a", "bf-b", "bf-c", "bf-d", "bf-e"] {
        upload(&fx, "noarch", name, "1.0").await;
    }
    let (_, built_index, _) = get(&fx, "noarch/repodata_shards.msgpack.zst").await;

    // Make the repository look as it did before the migration.
    for sql in [
        "DELETE FROM conda_shard_index WHERE repository_id = $1",
        "DELETE FROM conda_shard_dirty WHERE repository_id = $1",
        "DELETE FROM conda_subdir_revisions WHERE repository_id = $1",
        "INSERT INTO conda_channel_state (repository_id, backfill_done_at) VALUES ($1, NULL) \
         ON CONFLICT (repository_id) DO UPDATE SET backfill_done_at = NULL, \
             backfill_cursor = NULL",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(fx.repo_id)
            .execute(&fx.pool)
            .await
            .unwrap();
    }

    // Not backfilled: still served, by the walk.
    let (status, walked, _) = get(&fx, "noarch/repodata_shards.msgpack.zst").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decode_index(&walked), decode_index(&built_index));
    assert!(index_rows(&fx, "noarch").await.is_empty());

    let cursor = || async {
        sqlx::query_as::<_, (Option<String>, Option<chrono::DateTime<chrono::Utc>>)>(
            "SELECT backfill_cursor, backfill_done_at FROM conda_channel_state \
              WHERE repository_id = $1",
        )
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap()
    };
    assert_eq!(
        shard_store::backfill_batch_sized(&fx.pool, fx.repo_id, 2)
            .await
            .unwrap(),
        Some(false)
    );
    let (saved, done) = cursor().await;
    assert_eq!(saved.as_deref(), Some("bf-b"));
    assert!(done.is_none());
    assert_eq!(index_rows(&fx, "noarch").await.len(), 2);

    // A later run resumes after the cursor.
    let mut batches = 1;
    while shard_store::backfill_batch_sized(&fx.pool, fx.repo_id, 2)
        .await
        .unwrap()
        == Some(false)
    {
        batches += 1;
    }
    assert_eq!(batches, 3, "five names in batches of two");
    let (_, done) = cursor().await;
    assert!(done.is_some());
    // Finishing gives every named subdir a revision, so revision 0 keeps
    // meaning "no row ever named this subdir".
    let state = shard_store::subdir_state(&fx.pool, fx.repo_id, "noarch")
        .await
        .unwrap();
    assert!(state.backfilled && state.revision > 0, "{state:?}");
    assert_eq!(index_rows(&fx, "noarch").await.len(), 5);

    let (status, stored, _) = get(&fx, "noarch/repodata_shards.msgpack.zst").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored, walked,
        "the stored index is the walk's, byte for byte"
    );
    for name in ["bf-a", "bf-e"] {
        assert_eq!(shard_of(&fx, "noarch", name).await.unwrap().len(), 1);
    }
}

/// A soft delete through any path (here a plain catalog update) removes the
/// package's index row once nothing live is left, and the subdir revision
/// moves so the kept documents are rebuilt.
#[tokio::test]
async fn a_soft_delete_drops_the_index_row_and_moves_the_revision_4173() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let file = upload(&fx, "noarch", "del-a", "1.0").await;
    upload(&fx, "noarch", "del-b", "1.0").await;
    let before = shard_store::subdir_state(&fx.pool, fx.repo_id, "noarch")
        .await
        .unwrap();
    sqlx::query("UPDATE artifacts SET is_deleted = true WHERE repository_id = $1 AND path = $2")
        .bind(fx.repo_id)
        .bind(format!("noarch/{file}"))
        .execute(&fx.pool)
        .await
        .unwrap();
    let after = shard_store::subdir_state(&fx.pool, fx.repo_id, "noarch")
        .await
        .unwrap();
    assert!(after.revision > before.revision);
    let rows = index_rows(&fx, "noarch").await;
    assert!(!rows.contains_key("del-a"));
    assert!(rows.contains_key("del-b"));
    let (_, body, _) = get(&fx, "noarch/repodata.json").await;
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["packages"].get(&file).is_none());
    assert!(doc["removed"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(file)));
}

/// A virtual over two hosted members serves the one member's stored shard
/// for a name only it publishes, and a merged shard for a name both do.
#[tokio::test]
async fn virtual_hosted_members_serve_stored_and_merged_shards_4173() {
    let Some(first) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let pool = first.pool.clone();
    let (second_id, second_key, second_dir) = tdh::create_repo(&pool, "local", "conda").await;
    tdh::grant_repo_access(&pool, second_id, first.user_id).await;
    let second = tdh::Fixture {
        pool: pool.clone(),
        user_id: first.user_id,
        username: first.username.clone(),
        repo_id: second_id,
        repo_key: second_key,
        state: tdh::build_state(pool.clone(), second_dir.to_str().unwrap()),
        storage_dir: second_dir,
    };
    let (virtual_id, virtual_key, virtual_dir) = tdh::create_repo(&pool, "virtual", "conda").await;
    tdh::grant_repo_access(&pool, virtual_id, first.user_id).await;
    for (member, priority) in [(first.repo_id, 1), (second.repo_id, 2)] {
        sqlx::query(
            "INSERT INTO virtual_repo_members (virtual_repo_id, member_repo_id, priority) \
             VALUES ($1, $2, $3)",
        )
        .bind(virtual_id)
        .bind(member)
        .bind(priority)
        .execute(&pool)
        .await
        .unwrap();
    }
    let only_first = upload(&first, "noarch", "vm-only", "1.0").await;
    let shared_first = upload(&first, "noarch", "vm-both", "1.0").await;
    let shared_second = upload(&second, "noarch", "vm-both", "2.0").await;

    let virt = tdh::Fixture {
        pool: pool.clone(),
        user_id: first.user_id,
        username: first.username.clone(),
        repo_id: virtual_id,
        repo_key: virtual_key,
        state: tdh::build_state(pool.clone(), virtual_dir.to_str().unwrap()),
        storage_dir: virtual_dir,
    };
    assert_eq!(
        shard_of(&virt, "noarch", "vm-only").await.unwrap(),
        vec![only_first]
    );
    let mut both = vec![shared_first, shared_second];
    both.sort();
    assert_eq!(shard_of(&virt, "noarch", "vm-both").await.unwrap(), both);
}

/// The invalidation event round-trips through the shared channel's parser.
#[test]
fn conda_invalidation_payload_round_trips_4174() {
    use crate::services::cache_invalidation::{
        conda_repodata_invalidation_payload, parse_invalidation_payload, InvalidationEvent,
    };
    let repo_id = uuid::Uuid::new_v4();
    let payload = conda_repodata_invalidation_payload(repo_id, &["noarch".to_string()]);
    assert_eq!(
        parse_invalidation_payload(&payload).unwrap(),
        InvalidationEvent::CondaRepodataInvalidated {
            repo_id,
            subdirs: vec!["noarch".to_string()],
        }
    );
    // An oversized list degrades to "every subdir".
    let many: Vec<String> = (0..2000).map(|i| format!("subdir-{i}")).collect();
    let payload = conda_repodata_invalidation_payload(repo_id, &many);
    assert_eq!(
        parse_invalidation_payload(&payload).unwrap(),
        InvalidationEvent::CondaRepodataInvalidated {
            repo_id,
            subdirs: Vec::new(),
        }
    );
}

/// #4173: the dirty mark is part of the writer's own transaction (visible to
/// it before commit, to nobody else, and gone with a rollback), and once a
/// write elsewhere commits, the superseded shard's address is never served
/// again, even though no conda code path ran after the write.
#[tokio::test]
async fn the_mark_rides_the_writers_transaction_and_old_addresses_go_4173() {
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let kept = upload(&fx, "noarch", "tx-a", "1.0").await;
    let pulled = upload(&fx, "noarch", "tx-a", "2.0").await;
    let (_, index, _) = get(&fx, "noarch/repodata_shards.msgpack.zst").await;
    let old_address = hex::encode(decode_index(&index).remove("tx-a").unwrap());
    assert_eq!(dirty_count(&fx).await, 0);

    let withdraw = "UPDATE artifacts SET quarantine_status = 'rejected' \
                     WHERE repository_id = $1 AND path = $2";
    let mut tx = fx.pool.begin().await.unwrap();
    sqlx::query(withdraw)
        .bind(fx.repo_id)
        .bind(format!("noarch/{pulled}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    let inside: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conda_shard_dirty WHERE repository_id = $1")
            .bind(fx.repo_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert!(inside > 0, "marked inside the writer's transaction");
    assert_eq!(dirty_count(&fx).await, 0, "invisible before commit");
    tx.rollback().await.unwrap();
    assert_eq!(
        dirty_count(&fx).await,
        0,
        "a rolled back write leaves no mark"
    );

    sqlx::query(withdraw)
        .bind(fx.repo_id)
        .bind(format!("noarch/{pulled}"))
        .execute(&fx.pool)
        .await
        .unwrap();
    let (status, _, _) = get(&fx, &format!("noarch/shards/{old_address}.msgpack.zst")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the shard that still listed the withdrawn build is not served"
    );
    assert_eq!(shard_of(&fx, "noarch", "tx-a").await.unwrap(), vec![kept]);
}

/// #4174: a virtual's merged documents follow a membership change and a
/// write in a member made outside the conda routes, with no conda rebuild
/// in between.
#[tokio::test]
async fn a_virtual_follows_membership_and_member_writes_4174() {
    let Some(first) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let pool = first.pool.clone();
    let (second_id, second_key, second_dir) = tdh::create_repo(&pool, "local", "conda").await;
    tdh::grant_repo_access(&pool, second_id, first.user_id).await;
    let second = tdh::Fixture {
        pool: pool.clone(),
        user_id: first.user_id,
        username: first.username.clone(),
        repo_id: second_id,
        repo_key: second_key,
        state: tdh::build_state(pool.clone(), second_dir.to_str().unwrap()),
        storage_dir: second_dir,
    };
    let (virtual_id, virtual_key, virtual_dir) = tdh::create_repo(&pool, "virtual", "conda").await;
    tdh::grant_repo_access(&pool, virtual_id, first.user_id).await;
    let add_member = |member: uuid::Uuid, priority: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO virtual_repo_members (virtual_repo_id, member_repo_id, priority) \
                 VALUES ($1, $2, $3)",
            )
            .bind(virtual_id)
            .bind(member)
            .bind(priority)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    add_member(first.repo_id, 1).await;
    let in_first = upload(&first, "noarch", "vf-a", "1.0").await;
    let in_second = upload(&second, "noarch", "vf-b", "1.0").await;
    let virt = tdh::Fixture {
        pool: pool.clone(),
        user_id: first.user_id,
        username: first.username.clone(),
        repo_id: virtual_id,
        repo_key: virtual_key,
        state: tdh::build_state(pool.clone(), virtual_dir.to_str().unwrap()),
        storage_dir: virtual_dir,
    };
    let listed = |body: bytes::Bytes| -> Vec<String> {
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let mut out: Vec<String> = doc["packages"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        out.sort();
        out
    };
    let (status, body, _) = get(&virt, "noarch/repodata.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed(body), vec![in_first.clone()]);
    assert_eq!(shard_of(&virt, "noarch", "vf-b").await, None);

    add_member(second.repo_id, 2).await;
    let (_, body, _) = get(&virt, "noarch/repodata.json").await;
    let mut both = vec![in_first.clone(), in_second.clone()];
    both.sort();
    assert_eq!(listed(body), both, "the new member is merged");
    assert_eq!(
        shard_of(&virt, "noarch", "vf-b").await.unwrap(),
        vec![in_second.clone()]
    );

    sqlx::query(
        "UPDATE artifacts SET quarantine_status = 'rejected' \
          WHERE repository_id = $1 AND path = $2",
    )
    .bind(second.repo_id)
    .bind(format!("noarch/{in_second}"))
    .execute(&pool)
    .await
    .unwrap();
    let (_, body, _) = get(&virt, "noarch/repodata.json").await;
    assert_eq!(listed(body), vec![in_first.clone()], "the withdrawal shows");
    assert_eq!(shard_of(&virt, "noarch", "vf-b").await, None);
}

/// #4174: a rebuild is announced on the conda channel only. On the shared
/// channel a 1.11.x replica, which does not know the event, would answer each
/// one with a conservative flush of its authorization caches.
#[tokio::test]
async fn rebuilds_are_announced_off_the_shared_channel_4174() {
    use crate::services::cache_invalidation::{
        CACHE_INVALIDATION_CHANNEL, CONDA_REPODATA_INVALIDATION_CHANNEL,
    };
    let Some(fx) = tdh::Fixture::setup("local", "conda").await else {
        return;
    };
    let mut shared = sqlx::postgres::PgListener::connect_with(&fx.pool)
        .await
        .unwrap();
    shared.listen(CACHE_INVALIDATION_CHANNEL).await.unwrap();
    let mut conda = sqlx::postgres::PgListener::connect_with(&fx.pool)
        .await
        .unwrap();
    conda
        .listen(CONDA_REPODATA_INVALIDATION_CHANNEL)
        .await
        .unwrap();

    upload(&fx, "noarch", "chan-a", "1.0").await;

    let repo = fx.repo_id.to_string();
    let mut announced = false;
    while let Ok(Ok(n)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), conda.recv()).await
    {
        if n.payload().contains(&repo) {
            assert!(n.payload().contains("conda_repodata_invalidated"));
            announced = true;
            break;
        }
    }
    assert!(announced, "the rebuild is announced on the conda channel");
    while let Ok(Ok(n)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), shared.recv()).await
    {
        assert!(
            !n.payload().contains("conda_repodata_invalidated"),
            "conda event on the shared channel: {}",
            n.payload()
        );
    }
}
