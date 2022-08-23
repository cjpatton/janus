use common::{create_test_tasks, generate_network_name, submit_measurements_and_verify_aggregate};
use janus_core::{
    hpke::{test_util::generate_test_hpke_config_and_private_key, HpkePrivateKey},
    test_util::install_test_trace_subscriber,
};
use janus_server::task::Task;
use monolithic_integration_test::janus::Janus;
use std::env::{self, VarError};
use url::Url;

mod common;

/// A pair of Janus instances, running somewhere, against which integration tests may be run.
struct JanusPair {
    /// The leader's view of the task configured in both Janus aggregators.
    leader_task: Task,
    /// The private key corresponding to the collector HPKE configuration in the task configured in
    /// both Janus aggregators.
    collector_private_key: HpkePrivateKey,

    /// Handle to the leader's resources, which are released on drop.
    leader: Janus,
    /// Handle to the helper's resources, which are released on drop.
    helper: Janus,
}

impl JanusPair {
    fn in_cluster_aggregator_url(namespace: &str) -> Url {
        Url::parse(&format!(
            "http://aggregator.{namespace}.svc.cluster.local:80"
        ))
        .unwrap()
    }

    /// Set up a new Janus test instance with the provided task. If the environment variables listed
    /// below are set, then the test instance is backed by an already-running Kubernetes cluster.
    /// Otherwise, an ephemeral, in-process instance of Janus is spawned. In either case, the Janus
    /// aggregators' API endpoints will be available on the local loopback interface, at the URLs in
    /// `Self.leader_task.aggregator_endpoints`.
    ///
    /// If connecting to the Kubernetes cluster API (k8s API, not DAP API) at an IP address over
    /// HTTPS (e.g., "https://127.0.0.1:42356"), then the `monolithic_integration_test` package must
    /// be built with the `kube-openssl` feature, as the default rustls can't validate IP addresses
    /// in certificates.
    /// e.g., `cargo test --features kube-openssl --package monolithic_integration_test`
    ///
    /// Environment variables:
    ///  - `JANUS_E2E_KUBE_CONFIG_PATH`: The path to a `kubectl` configuration file containing
    ///    the information needed to connect to the Kubernetes cluster where the test is to be run.
    ///  - `JANUS_E2E_KUBECTL_CONTEXT_NAME`: The name of a `kubectl` context defined in the config
    ///    at `JANUS_E2E_KUBE_CONFIG_PATH`. The context will be used to connect to the Kubernetes
    ///    cluster where the test is to be run. The context should grant sufficient cluster
    ///    permissions to view secrets and forward ports to services.
    ///  - `JANUS_E2E_LEADER_NAMESPACE`: The Kubernetes namespace where the DAP leader is deployed.
    ///  - `JANUS_E2E_HELPER_NAMESPACE`: The Kubernetes namespace where the DAP helper is deployed.
    pub async fn new() -> Self {
        let (collector_hpke_config, collector_private_key) =
            generate_test_hpke_config_and_private_key();
        let (mut leader_task, mut helper_task) = create_test_tasks(&collector_hpke_config);

        // The environment variables should either all be present, or all be absent
        let (leader, helper) = match (
            env::var("JANUS_E2E_KUBE_CONFIG_PATH"),
            env::var("JANUS_E2E_KUBECTL_CONTEXT_NAME"),
            env::var("JANUS_E2E_LEADER_NAMESPACE"),
            env::var("JANUS_E2E_HELPER_NAMESPACE"),
        ) {
            (
                Ok(kubeconfig_path),
                Ok(kubectl_context_name),
                Ok(leader_namespace),
                Ok(helper_namespace),
            ) => {
                // From outside the cluster, the aggregators are reached at "localhost:<port>",
                // where "port" is whatever unused port we use with `kubectl port-forward`. But when
                // the aggregators talk to each other, they do it on the cluster's private network,
                // and so they need the in-cluster DNS name of the other aggregator. However, since
                // aggregators use the endpoint URLs in the task to construct collect job URIs, we
                // must only fix the _peer_ aggregator's endpoint.
                leader_task.aggregator_endpoints[1] =
                    Self::in_cluster_aggregator_url(&helper_namespace);
                let leader = Janus::new_with_kubernetes_cluster(
                    &kubeconfig_path,
                    &kubectl_context_name,
                    &leader_namespace,
                    &leader_task,
                )
                .await;

                helper_task.aggregator_endpoints[0] =
                    Self::in_cluster_aggregator_url(&leader_namespace);
                let helper = Janus::new_with_kubernetes_cluster(
                    &kubeconfig_path,
                    &kubectl_context_name,
                    &helper_namespace,
                    &helper_task,
                )
                .await;

                // Wait just a bit to allow kubectl port-forwards to be ready
                // TODO(#196): Remove this.
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                (leader, helper)
            }
            (
                Err(VarError::NotPresent),
                Err(VarError::NotPresent),
                Err(VarError::NotPresent),
                Err(VarError::NotPresent),
            ) => {
                let network = generate_network_name();
                (
                    Janus::new_in_container(&network, &leader_task).await,
                    Janus::new_in_container(&network, &helper_task).await,
                )
            }
            _ => panic!("unexpected environment variables"),
        };

        Self {
            leader_task,
            collector_private_key,
            leader,
            helper,
        }
    }
}

// This test places Janus in both the leader & helper roles.
#[tokio::test(flavor = "multi_thread")]
async fn janus_janus() {
    install_test_trace_subscriber();

    // Start servers.
    let janus_pair = JanusPair::new().await;

    // Run the behavioral test.
    submit_measurements_and_verify_aggregate(
        (janus_pair.leader.port(), janus_pair.helper.port()),
        &janus_pair.leader_task,
        &janus_pair.collector_private_key,
    )
    .await;
}