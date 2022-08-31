//! Functionality for tests interacting with Daphne (<https://github.com/cloudflare/daphne>).

use daphne::DapGlobalConfig;
use interop_binaries::test_util::{await_http_server, load_zstd_compressed_docker_image};
use janus_core::message::{HpkeAeadId, HpkeConfig, HpkeKdfId, HpkeKemId, Role};
use janus_server::task::{Task, VdafInstance};
use portpicker::pick_unused_port;
use rand::{thread_rng, Rng};
use reqwest::Url;
use serde::Serialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{mpsc, Mutex},
    time::Duration,
};
use testcontainers::{
    clients::Cli, core::Port, images::generic::GenericImage, Container, RunnableImage,
};
use tokio::{select, sync::oneshot, task, time::interval};

// test_daphne.metadata / test_daphne.tar.zst are generated by this package's build.rs.
static TEST_DAPHNE_IMAGE_NAME_AND_TAG: Mutex<Option<(String, String)>> = Mutex::new(None);
const TEST_DAPHNE_METADATA_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/test_daphne.metadata"));
const TEST_DAPHNE_IMAGE_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/test_daphne.tar.zst"));

/// Represents a running Daphne test instance.
pub struct Daphne<'a> {
    daphne_container: Container<'a, GenericImage>,

    // Task lifetime management.
    start_shutdown_sender: Option<oneshot::Sender<()>>,
    shutdown_complete_receiver: Option<mpsc::Receiver<()>>,
}

impl<'a> Daphne<'a> {
    /// Create & start a new hermetic Daphne test instance in the given Docker network, configured
    /// to service the given task. The aggregator port is also exposed to the host.
    pub async fn new(container_client: &'a Cli, network: &str, task: &Task) -> Daphne<'a> {
        // Generate values needed for the Daphne environment configuration based on the provided
        // Janus task definition.

        // Daphne's DAP global config configures a few Daphne-specific configuration parameters.
        // These aren't part of the DAP-specific task parameters, so we give reasonable defaults
        // that work for our testing purposes.
        let dap_global_config = DapGlobalConfig {
            max_batch_duration: 360000,
            min_batch_interval_start: 259200,
            max_batch_interval_end: 259200,
        };

        // Daphne currently only supports an HPKE config of (X25519HkdfSha256, HkdfSha256,
        // Aes128Gcm); this is checked in `DaphneHpkeConfig::from`.
        let dap_hpke_receiver_config_list = serde_json::to_string(
            &task
                .hpke_keys
                .values()
                .map(|(hpke_config, private_key)| DaphneHpkeReceiverConfig {
                    config: DaphneHpkeConfig::from(hpke_config.clone()),
                    secret_key: hex::encode(private_key.as_ref()),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();

        // The DAP bucket key is an internal, private key used to map client reports to internal
        // storage buckets.
        let mut dap_bucket_key = [0; 16];
        thread_rng().fill(&mut dap_bucket_key);

        // The DAP collect ID key is an internal, private key used to map collect requests to a
        // collect job ID. (It's only used when Daphne is in the Leader role, but we populate it
        // either way.)
        let mut dap_collect_id_key = [0; 16];
        thread_rng().fill(&mut dap_collect_id_key);

        let dap_task_list = serde_json::to_string(&HashMap::from([(
            hex::encode(task.id.as_bytes()),
            DaphneDapTaskConfig {
                version: "v01".to_string(),
                leader_url: task.aggregator_url(Role::Leader).unwrap().clone(),
                helper_url: task.aggregator_url(Role::Helper).unwrap().clone(),
                min_batch_duration: task.min_batch_duration.as_seconds(),
                min_batch_size: task.min_batch_size,
                vdaf: daphne_vdaf_config_from_janus_vdaf(&task.vdaf),
                vdaf_verify_key: hex::encode(task.vdaf_verify_keys().first().unwrap().as_bytes()),
                collector_hpke_config: DaphneHpkeConfig::from(task.collector_hpke_config.clone()),
            },
        )]))
        .unwrap();

        // Daphne currently only supports one auth token per task. Janus supports multiple tokens
        // per task to allow rotation; we supply Daphne with the "primary" token.
        let aggregator_bearer_token_list = json!({
            hex::encode(task.id.as_bytes()): String::from_utf8(task.primary_aggregator_auth_token().as_bytes().to_vec()).unwrap()
        }).to_string();
        let collector_bearer_token_list = if task.role == Role::Leader {
            json!({
                hex::encode(task.id.as_bytes()): String::from_utf8(task.primary_collector_auth_token().as_bytes().to_vec()).unwrap()
            }).to_string()
        } else {
            String::new()
        };

        // Get the test Daphne docker image hash; if necessary, do one-time setup to determine the
        // image name & tag (which may involve writing the image to docker depending on compilation
        // settings).
        let (image_name, image_tag) = {
            let mut image_name_and_tag = TEST_DAPHNE_IMAGE_NAME_AND_TAG.lock().unwrap();
            if image_name_and_tag.is_none() {
                let metadata: serde_json::Value =
                    serde_json::from_slice(TEST_DAPHNE_METADATA_BYTES)
                        .expect("Couldn't parse Daphne test image metadata");
                let strategy = metadata["strategy"].as_str().unwrap_or_else(|| {
                    panic!(
                        "Metadata strategy field was not a string: {}",
                        metadata["strategy"]
                    )
                });

                *image_name_and_tag = Some(match strategy {
                    "build" => (
                        "sha256".to_string(),
                        load_zstd_compressed_docker_image(TEST_DAPHNE_IMAGE_BYTES),
                    ),

                    "prebuilt" => (
                        metadata["image_name"]
                            .as_str()
                            .unwrap_or_else(|| {
                                panic!(
                                    "Daphne test image metadata image_name field was not a string: {}",
                                    metadata["image_name"]
                                )
                            })
                            .to_string(),
                        metadata["image_tag"]
                            .as_str()
                            .unwrap_or_else(|| {
                                panic!(
                                    "Daphne test image metadata image_tag field was not a string: {}",
                                    metadata["image_tag"]
                                )
                            })
                            .to_string(),
                    ),

                    "skip" => panic!("No Daphne test image available (compiled with DAPHNE_INTEROP_CONTAINER=skip)"),

                    _ => panic!("Unknown Daphne test image build strategy: {strategy:?}"),
                });
            }
            image_name_and_tag.as_ref().unwrap().clone()
        };

        // Start the Daphne test container running.
        let port = pick_unused_port().expect("Couldn't pick unused port");
        let endpoint = task.aggregator_url(task.role).unwrap();

        let args = [
            (
                // Note: DAP_DEPLOYMENT=dev overrides aggregator endpoint hostnames to "localhost",
                // so it can't be used. The other option is DAP_DEPLOYMENT=prod -- despite the name,
                // that's what we want since it operates as configured without any special dev-only
                // overrides that our test configuration doesn't need.
                "DAP_DEPLOYMENT".to_string(),
                "prod".to_string(),
            ),
            (
                // Works around https://github.com/cloudflare/daphne/issues/73. Remove once
                // that issue is closed & the version of Daphne under test has picked up the fix.
                "DAP_ISSUE73_DISABLE_AGG_JOB_QUEUE_GARBAGE_COLLECTION".to_string(),
                "true".to_string(),
            ),
            (
                "DAP_AGGREGATOR_ROLE".to_string(),
                task.role.as_str().to_string(),
            ),
            (
                "DAP_GLOBAL_CONFIG".to_string(),
                serde_json::to_string(&dap_global_config).unwrap(),
            ),
            (
                "DAP_HPKE_RECEIVER_CONFIG_LIST".to_string(),
                dap_hpke_receiver_config_list,
            ),
            ("DAP_BUCKET_KEY".to_string(), hex::encode(&dap_bucket_key)),
            ("DAP_BUCKET_COUNT".to_string(), "2".to_string()),
            (
                "DAP_COLLECT_ID_KEY".to_string(),
                hex::encode(&dap_collect_id_key),
            ),
            ("DAP_TASK_LIST".to_string(), dap_task_list),
            (
                "DAP_LEADER_BEARER_TOKEN_LIST".to_string(),
                aggregator_bearer_token_list,
            ),
            (
                "DAP_COLLECTOR_BEARER_TOKEN_LIST".to_string(),
                collector_bearer_token_list,
            ),
        ]
        .into_iter()
        .map(|(env_var, env_val)| format!("--binding={env_var}={env_val}"));
        let args = ["--port=8080".to_string()]
            .into_iter()
            .chain(args)
            .collect();
        let runnable_image =
            RunnableImage::from((GenericImage::new(&image_name, &image_tag), args))
                .with_network(network)
                .with_container_name(endpoint.host_str().unwrap())
                .with_mapped_port(Port {
                    local: port,
                    internal: 8080,
                });
        let daphne_container = container_client.run(runnable_image);

        // Wait for Daphne container to begin listening on the port.
        await_http_server(port).await;

        // Set up a task that occasionally hits the /internal/process endpoint, which is required
        // for Daphne to progress aggregations. (this is only required if Daphne is in the Leader
        // role, but for simplicity we hit the endpoint either way -- the resulting 404's do not
        // cause problems if Daphne is acting as the helper)
        let (start_shutdown_sender, mut start_shutdown_receiver) = oneshot::channel();
        let (shutdown_complete_sender, shutdown_complete_receiver) = mpsc::channel();
        task::spawn({
            let http_client = reqwest::Client::default();
            let mut request_url = task
                .aggregator_url(task.role)
                .unwrap()
                .join("/internal/process")
                .unwrap();
            request_url.set_host(Some("localhost")).unwrap();
            request_url.set_port(Some(port)).unwrap();

            let mut interval = interval(Duration::from_millis(250));
            async move {
                loop {
                    select! {
                        _ = interval.tick() => (),
                        _ = &mut start_shutdown_receiver => {
                            shutdown_complete_sender.send(()).unwrap();
                            return;
                        },
                    }

                    // The body is a JSON-encoding of Daphne's `InternalAggregateInfo`.
                    let _ = http_client
                        .post(request_url.clone())
                        .json(&json!({
                            "max_buckets": 1000,
                            "max_reports": 1000,
                        }))
                        .send()
                        .await;
                }
            }
        });

        Self {
            daphne_container,
            start_shutdown_sender: Some(start_shutdown_sender),
            shutdown_complete_receiver: Some(shutdown_complete_receiver),
        }
    }

    /// Returns the port of the aggregator on the host.
    pub fn port(&self) -> u16 {
        self.daphne_container.get_host_port_ipv4(8080)
    }
}

impl<'a> Drop for Daphne<'a> {
    fn drop(&mut self) {
        let start_shutdown_sender = self.start_shutdown_sender.take().unwrap();
        let shutdown_complete_receiver = self.shutdown_complete_receiver.take().unwrap();
        start_shutdown_sender.send(()).unwrap();
        shutdown_complete_receiver.recv().unwrap();
    }
}

fn daphne_vdaf_config_from_janus_vdaf(vdaf: &VdafInstance) -> daphne::VdafConfig {
    match vdaf {
        VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Count) => {
            daphne::VdafConfig::Prio3(daphne::Prio3Config::Count)
        }

        VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Histogram { buckets }) => {
            daphne::VdafConfig::Prio3(daphne::Prio3Config::Histogram {
                buckets: buckets.clone(),
            })
        }

        VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Sum { bits }) => {
            daphne::VdafConfig::Prio3(daphne::Prio3Config::Sum { bits: *bits })
        }

        _ => panic!("Unsupported VdafInstance: {:?}", vdaf),
    }
}

// Corresponds to Daphne's `HpkeReceiverConfig`. We can't use that type directly as some of the
// fields we need to populate (e.g. `secret_key`) are not public.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DaphneHpkeReceiverConfig {
    config: DaphneHpkeConfig,
    secret_key: String,
}

// Corresponds to Daphne's `HpkeConfig`. We can't use that type directly as some of the fields we
// need to populate (e.g. `public_key`) are not public.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DaphneHpkeConfig {
    id: u8,
    kem_id: HpkeKemId,
    kdf_id: HpkeKdfId,
    aead_id: HpkeAeadId,
    public_key: String,
}

impl From<HpkeConfig> for DaphneHpkeConfig {
    fn from(hpke_config: HpkeConfig) -> Self {
        // Daphne currently only supports this specific HPKE configuration, so make sure that we
        // are converting something Daphne can use.
        assert_eq!(hpke_config.kem_id(), HpkeKemId::X25519HkdfSha256);
        assert_eq!(hpke_config.kdf_id(), HpkeKdfId::HkdfSha256);
        assert_eq!(hpke_config.aead_id(), HpkeAeadId::Aes128Gcm);

        DaphneHpkeConfig {
            id: u8::from(hpke_config.id()),
            kem_id: hpke_config.kem_id(),
            kdf_id: hpke_config.kdf_id(),
            aead_id: hpke_config.aead_id(),
            public_key: hex::encode(hpke_config.public_key().as_bytes()),
        }
    }
}

// Corresponds to Daphne's `DapTaskConfig`. We can't use that type directly as some of the fields we
// need to populate (e.g. `vdaf_verify_key`) are not public.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DaphneDapTaskConfig {
    pub version: String,
    pub leader_url: Url,
    pub helper_url: Url,
    pub min_batch_duration: u64,
    pub min_batch_size: u64,
    pub vdaf: daphne::VdafConfig,
    pub vdaf_verify_key: String,
    pub collector_hpke_config: DaphneHpkeConfig,
}
