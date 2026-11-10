//! #4498: a repository the caller may not read answers exactly like a
//! repository that does not exist, on every REST read endpoint that hides
//! existence.
//!
//! Router-level: every request goes through `create_router`, so the auth
//! middleware, `require_visible` and `RepositoryService::get_by_key` are the
//! production ones. For each endpoint the test compares the WHOLE response
//! (status and body bytes) for a private repository against the response for
//! a key that names no repository, for an anonymous caller and for an
//! authenticated user without a grant. A user holding a read grant reads the
//! repository.

#[cfg(ak_test_shard = "router")]
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use bytes::Bytes;
    use uuid::Uuid;

    use crate::api::handlers::test_db_helpers as tdh;
    use crate::api::SharedState;

    /// Endpoints that read a repository by key. `{}` is the key.
    const READ_ENDPOINTS: &[&str] = &[
        "/api/v1/repositories/{}",
        "/api/v1/repositories/{}/artifacts",
        "/api/v1/repositories/{}/artifacts/app/1.0/app.bin",
        "/api/v1/repositories/{}/download/app/1.0/app.bin",
        "/api/v1/repositories/{}/storage",
        "/api/v1/repositories/{}/versions/app/1.0/app.bin",
        "/api/v1/promotion/repositories/{}/promotion-history",
        "/api/v1/promotion/repositories/{}/release-target",
    ];

    struct Rig {
        pool: sqlx::PgPool,
        state: SharedState,
        repo_id: Uuid,
        repo_key: String,
        reader: Uuid,
        outsider: Uuid,
    }

    impl Rig {
        async fn new(pool: sqlx::PgPool) -> Self {
            // A private staging repository: the promotion endpoints read
            // staging repositories, and every other endpoint reads any type.
            let (repo_id, repo_key, dir) = tdh::create_repo(&pool, "staging", "generic").await;
            let (reader, _) = tdh::create_user(&pool).await;
            let (outsider, _) = tdh::create_user(&pool).await;
            tdh::grant_repo_role(&pool, repo_id, reader, "reader").await;
            let state = tdh::build_state(pool.clone(), dir.to_str().expect("utf8"));
            Self {
                pool,
                state,
                repo_id,
                repo_key,
                reader,
                outsider,
            }
        }

        async fn get(&self, bearer: Option<&str>, uri: String) -> (StatusCode, Bytes) {
            let app = crate::api::routes::create_router(self.state.clone());
            let mut req = Request::builder().method("GET").uri(uri);
            if let Some(b) = bearer {
                req = req.header("authorization", b);
            }
            tdh::send(app, req.body(Body::empty()).expect("request")).await
        }

        async fn teardown(self) {
            tdh::cleanup(&self.pool, self.repo_id, self.reader).await;
            tdh::cleanup_user(&self.pool, self.outsider).await;
        }
    }

    fn uri(template: &str, key: &str) -> String {
        template.replace("{}", key)
    }

    /// Anonymous and grant-less callers cannot tell a private repository from
    /// a missing one: same status, same body, on every read endpoint.
    #[tokio::test]
    async fn unreadable_repo_answers_like_a_missing_one_4498() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let rig = Rig::new(pool).await;
        let outsider = tdh::bearer_for(&rig.state, rig.outsider).await;
        let missing_key = format!("ph-4498-missing-{}", Uuid::new_v4());

        for (who, bearer) in [("anonymous", None), ("no grant", Some(outsider.as_str()))] {
            for template in READ_ENDPOINTS {
                let (s_hidden, b_hidden) = rig.get(bearer, uri(template, &rig.repo_key)).await;
                let (s_missing, b_missing) = rig.get(bearer, uri(template, &missing_key)).await;
                assert_ne!(
                    s_hidden,
                    StatusCode::OK,
                    "{who}: {template} must not serve a private repository"
                );
                assert_eq!(
                    (s_hidden, &b_hidden),
                    (s_missing, &b_missing),
                    "{who}: {template} must answer an unreadable repository exactly like a \
                     missing one (hidden {:?} vs missing {:?})",
                    String::from_utf8_lossy(&b_hidden),
                    String::from_utf8_lossy(&b_missing),
                );
                let body = String::from_utf8_lossy(&b_hidden);
                assert!(
                    !body.contains(&rig.repo_key),
                    "{who}: {template} must not echo the hidden key: {body}"
                );
            }
        }
        rig.teardown().await;
    }

    /// The control: a user with a read grant reads the same repository.
    #[tokio::test]
    async fn reader_reads_the_private_repo_4498() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let rig = Rig::new(pool).await;
        let reader = tdh::bearer_for(&rig.state, rig.reader).await;
        for template in [
            "/api/v1/repositories/{}",
            "/api/v1/repositories/{}/artifacts",
            "/api/v1/promotion/repositories/{}/promotion-history",
        ] {
            let (status, body) = rig.get(Some(&reader), uri(template, &rig.repo_key)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "reader: {template}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        rig.teardown().await;
    }
}
