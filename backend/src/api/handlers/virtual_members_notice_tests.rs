//! #4690: a user granted read on a virtual repository but on none of its
//! members gets an empty listing, as the access model intends (a grant on a
//! virtual does not grant its members, #1804 / #3163). What changed is that
//! the REST listings now say why, in `X-AK-Virtual-Members-Notice`, instead of
//! leaving only a server log line. The format routes still answer 404.
//!
//! Router-level, so the production middleware, `require_visible` gates and
//! member walks are exercised. Fixture (all PRIVATE):
//!
//! ```text
//! maven-virtual = [maven-hosted (local, one jar)]
//! empty-virtual = []
//! ```
//!
//! `reader` is in a group holding `read` on both virtuals and nothing else,
//! exactly the reporter's setup.

#[cfg(ak_test_shard = "router")]
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use bytes::Bytes;
    use std::sync::Arc;
    use uuid::Uuid;

    use crate::api::handlers::repositories::{
        NO_READABLE_MEMBERS_NOTICE, VIRTUAL_MEMBERS_NOTICE_HEADER,
    };
    use crate::api::handlers::test_db_helpers as tdh;
    use crate::api::SharedState;
    use crate::services::auth_service::AuthService;

    const JAR_PATH: &str = "com/acme/core/1.0.0/core-1.0.0.jar";
    const JAR_BYTES: &[u8] = b"hosted-core-jar-bytes";

    struct Repo {
        id: Uuid,
        key: String,
        dir: std::path::PathBuf,
    }

    async fn repo(pool: &sqlx::PgPool, repo_type: &str) -> Repo {
        let (id, key, dir) = tdh::create_repo(pool, repo_type, "maven").await;
        Repo { id, key, dir }
    }

    struct Reply {
        status: StatusCode,
        notice: Option<String>,
        json: serde_json::Value,
    }

    struct Rig {
        pool: sqlx::PgPool,
        state: SharedState,
        virt: Repo,
        empty: Repo,
        hosted: Repo,
        group: Uuid,
        reader: Uuid,
        token: String,
        _root: tempfile::TempDir,
    }

    impl Rig {
        async fn new(pool: sqlx::PgPool) -> Self {
            let virt = repo(&pool, "virtual").await;
            let empty = repo(&pool, "virtual").await;
            let hosted = repo(&pool, "local").await;
            tdh::link_virtual_member(&pool, virt.id, hosted.id, 1).await;

            let (reader, _) = tdh::create_user(&pool).await;
            let (group, _) = tdh::create_group(&pool).await;
            sqlx::query("INSERT INTO user_group_members (user_id, group_id) VALUES ($1, $2)")
                .bind(reader)
                .bind(group)
                .execute(&pool)
                .await
                .expect("add reader to the group");
            for r in [&virt, &empty] {
                tdh::grant_permission(&pool, "group", group, "repository", r.id, &["read"]).await;
            }

            let root = tempfile::tempdir().expect("storage root");
            let root_path = root.path().to_str().expect("utf8").to_string();
            let proxy = tdh::build_proxy_service_with_fs(pool.clone(), &root_path);
            let state = tdh::build_state_with_proxy(pool.clone(), &root_path, proxy);
            let info = tdh::make_repo_info(hosted.id, &hosted.key, &hosted.dir, "local", None);
            tdh::seed_artifact(
                &state,
                &pool,
                &info,
                &format!("maven/{JAR_PATH}"),
                JAR_PATH,
                "com.acme:core",
                "1.0.0",
                "application/java-archive",
                Bytes::from_static(JAR_BYTES),
                reader,
            )
            .await;
            let (token, _) = AuthService::new(pool.clone(), Arc::new(state.config.clone()))
                .generate_api_token(
                    reader,
                    "virtual-members-notice",
                    vec![
                        "read:artifacts".to_string(),
                        "read:repositories".to_string(),
                    ],
                    None,
                )
                .await
                .expect("mint API token");
            Self {
                pool,
                state,
                virt,
                empty,
                hosted,
                group,
                reader,
                token,
                _root: root,
            }
        }

        async fn get(&self, uri: String) -> (Reply, Bytes) {
            let app = crate::api::routes::create_router(self.state.clone());
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {}", self.token))
                .body(Body::empty())
                .expect("request");
            let (status, body, headers) = tdh::send_with_headers(app, req).await;
            let notice = headers
                .get(VIRTUAL_MEMBERS_NOTICE_HEADER)
                .map(|v| v.to_str().expect("ascii header").to_string());
            let json = serde_json::from_slice(&body).unwrap_or_default();
            (
                Reply {
                    status,
                    notice,
                    json,
                },
                body,
            )
        }

        async fn members(&self, r: &Repo) -> Reply {
            self.get(format!("/api/v1/repositories/{}/members", r.key))
                .await
                .0
        }

        async fn artifacts(&self, r: &Repo) -> Reply {
            self.get(format!("/api/v1/repositories/{}/artifacts", r.key))
                .await
                .0
        }

        async fn maven(&self, r: &Repo) -> (StatusCode, Bytes) {
            let (reply, body) = self.get(format!("/maven/{}/{JAR_PATH}", r.key)).await;
            (reply.status, body)
        }

        async fn cleanup(self) {
            let _ = sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
                .bind(self.reader)
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM permissions WHERE principal_id = $1")
                .bind(self.group)
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM groups WHERE id = $1")
                .bind(self.group)
                .execute(&self.pool)
                .await;
            for r in [&self.virt, &self.empty, &self.hosted] {
                let _ = sqlx::query("DELETE FROM virtual_repo_members WHERE virtual_repo_id = $1")
                    .bind(r.id)
                    .execute(&self.pool)
                    .await;
                tdh::cleanup_member_repo(&self.pool, r.id, &r.dir).await;
            }
            tdh::cleanup_user(&self.pool, self.reader).await;
        }
    }

    fn member_count(reply: &Reply) -> usize {
        reply.json["members"]
            .as_array()
            .map_or(usize::MAX, Vec::len)
    }

    fn item_paths(reply: &Reply) -> Vec<String> {
        reply.json["items"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|a| a["path"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn read_grant_on_virtual_alone_gets_an_explained_empty_listing_4690() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let rig = Rig::new(pool).await;

        // Access is unchanged: the member stays unreadable through the virtual.
        let (status, _) = rig.maven(&rig.virt).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "format route still 404s");

        // ...but the REST listings now say why they are empty.
        let members = rig.members(&rig.virt).await;
        assert_eq!(members.status, StatusCode::OK);
        assert_eq!(member_count(&members), 0, "no member is listed");
        assert_eq!(
            members.notice.as_deref(),
            Some(NO_READABLE_MEMBERS_NOTICE),
            "the member listing explains the empty result"
        );
        let artifacts = rig.artifacts(&rig.virt).await;
        assert_eq!(artifacts.status, StatusCode::OK);
        assert!(item_paths(&artifacts).is_empty(), "{:?}", artifacts.json);
        assert_eq!(
            artifacts.notice.as_deref(),
            Some(NO_READABLE_MEMBERS_NOTICE),
            "the artifact listing explains the empty result"
        );

        // #3452: a virtual with no members at all answers the same, so the
        // notice does not reveal that private members exist.
        let empty_members = rig.members(&rig.empty).await;
        assert_eq!(empty_members.status, StatusCode::OK);
        assert_eq!(member_count(&empty_members), 0);
        assert_eq!(empty_members.notice, members.notice);
        assert_eq!(empty_members.json, members.json);
        let empty_artifacts = rig.artifacts(&rig.empty).await;
        assert_eq!(empty_artifacts.notice, artifacts.notice);

        // Following the notice's advice clears it: with read on the member,
        // the member is listed and served through the virtual, and no notice.
        tdh::grant_permission(
            &rig.pool,
            "group",
            rig.group,
            "repository",
            rig.hosted.id,
            &["read"],
        )
        .await;
        rig.state.permission_service.invalidate_cache();
        let members = rig.members(&rig.virt).await;
        assert_eq!(member_count(&members), 1);
        assert_eq!(members.notice, None);
        let artifacts = rig.artifacts(&rig.virt).await;
        assert_eq!(item_paths(&artifacts), vec![JAR_PATH.to_string()]);
        assert_eq!(artifacts.notice, None);
        let (status, body) = rig.maven(&rig.virt).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], JAR_BYTES);

        rig.cleanup().await;
    }
}
