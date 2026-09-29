//! Contract for the Adaptive ECR publish workflow.
//!
//! `.github/workflows/ecr-publish.yml` is the only image publisher in this
//! fork. These tests pin the routing and the build so a later edit cannot
//! silently publish the wrong registry, a second image, or a Helm release.
//!
//! CI runs this target from the Tier 1 integration step. The tests are not
//! `#[ignore]`d.

use serde_yaml::Value;
use std::path::Path;

fn load_workflow() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".github")
        .join("workflows")
        .join("ecr-publish.yml");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_yaml::from_str(&raw).expect("ecr-publish.yml is not valid YAML")
}

fn raw_workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".github")
        .join("workflows")
        .join("ecr-publish.yml");
    std::fs::read_to_string(path).expect("read ecr-publish.yml")
}

#[test]
fn publish_workflow_builds_only_the_backend_and_does_not_deploy() {
    let workflow = load_workflow();
    let raw = raw_workflow();
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("jobs");
    assert_eq!(jobs.len(), 1, "the publish workflow has one job");
    assert!(jobs.contains_key(Value::String("publish".to_string())));

    assert!(
        raw.contains("file: docker/Dockerfile.backend"),
        "the build must use the backend Dockerfile"
    );
    assert!(
        raw.contains("platforms: linux/arm64"),
        "the image is built natively for arm64"
    );
    assert!(raw.contains("runner=artifact_keeper_build"));
    assert!(raw.contains("spot=false"));
    assert!(
        !raw.contains("ubuntu-24.04"),
        "the image build does not use a GitHub-hosted runner"
    );
    assert!(
        raw.contains("ECR_REPOSITORY: artifact-keeper-fork"),
        "the image repository is artifact-keeper-fork"
    );
    assert!(
        !raw.contains("helm ") && !raw.contains("deploy-to-eks"),
        "fork CI pushes the image and does not Helm-deploy"
    );
    for other in [
        "Dockerfile.web",
        "Dockerfile.openscap",
        "Dockerfile.scanner",
        "grype",
        "trivy",
    ] {
        assert!(
            !raw.contains(other),
            "the publish workflow must not build {other}"
        );
    }
}

#[test]
fn feature_branches_publish_to_infra_dev_and_main_publishes_to_infra_prod() {
    let raw = raw_workflow();
    assert!(raw.contains("INFRA_DEV_AWS_ACCOUNT_ID: \"091974775961\""));
    assert!(raw.contains("INFRA_PROD_AWS_ACCOUNT_ID: \"756418066679\""));
    assert!(
        raw.contains("refs/heads/main") && raw.contains("target=\"prod\""),
        "main publishes to infra-prod"
    );
    assert!(
        raw.contains("target=\"dev\""),
        "every other ref publishes to infra-dev"
    );
    assert!(
        raw.contains("${GITHUB_SHA}"),
        "the image tag is the full git SHA"
    );
}

#[test]
fn publish_workflow_does_not_cancel_itself() {
    let raw = raw_workflow();
    let mut in_block = false;
    let mut cancel = None;
    for line in raw.lines() {
        if line.starts_with("concurrency:") {
            in_block = true;
            continue;
        }
        if in_block {
            if !line.starts_with(' ') && !line.trim().is_empty() {
                break;
            }
            if let Some(rest) = line.trim().strip_prefix("cancel-in-progress:") {
                cancel = Some(rest.trim().to_string());
            }
        }
    }
    assert_eq!(cancel.as_deref(), Some("false"));
}
