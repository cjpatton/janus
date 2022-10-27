use base64::URL_SAFE_NO_PAD;
use janus_aggregator::task::{QueryType, Task, VdafInstance};
use janus_core::hpke::{generate_hpke_config_and_private_key, HpkePrivateKey};
use janus_messages::{
    query_type::{FixedSize, QueryType as _, TimeInterval},
    HpkeAeadId, HpkeConfig, HpkeConfigId, HpkeKdfId, HpkeKemId, Role,
};
use prio::codec::Encode;
use rand::random;
use serde::{de::Visitor, Deserialize, Serialize};
use std::{
    collections::HashMap,
    env::{self, VarError},
    fmt::Display,
    marker::PhantomData,
    path::PathBuf,
    str::FromStr,
};
use tracing_log::LogTracer;
use tracing_subscriber::{prelude::*, EnvFilter, Registry};
use url::Url;

#[cfg(feature = "testcontainer")]
pub mod testcontainer;

pub mod status {
    pub static SUCCESS: &str = "success";
    pub static ERROR: &str = "error";
    pub static COMPLETE: &str = "complete";
    pub static IN_PROGRESS: &str = "in progress";
}

/// Helper type to serialize/deserialize a large number as a string.
#[derive(Debug, Clone)]
pub struct NumberAsString<T>(pub T);

impl<T> Serialize for NumberAsString<T>
where
    T: Display,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de, T> Deserialize<'de> for NumberAsString<T>
where
    T: FromStr,
    <T as FromStr>::Err: Display,
{
    fn deserialize<D>(deserializer: D) -> Result<NumberAsString<T>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(NumberAsStringVisitor::new())
    }
}

struct NumberAsStringVisitor<T>(PhantomData<T>);

impl<T> NumberAsStringVisitor<T> {
    fn new() -> NumberAsStringVisitor<T> {
        NumberAsStringVisitor(PhantomData)
    }
}

impl<'de, T> Visitor<'de> for NumberAsStringVisitor<T>
where
    T: FromStr,
    <T as FromStr>::Err: Display,
{
    type Value = NumberAsString<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a string with a number in base 10")
    }

    fn visit_str<E>(self, value: &str) -> Result<NumberAsString<T>, E>
    where
        E: serde::de::Error,
    {
        let number = value
            .parse()
            .map_err(|err| E::custom(format!("string could not be parsed into number: {}", err)))?;
        Ok(NumberAsString(number))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VdafObject {
    Prio3Aes128Count,
    Prio3Aes128CountVec { length: NumberAsString<usize> },
    Prio3Aes128Sum { bits: NumberAsString<u32> },
    Prio3Aes128Histogram { buckets: Vec<NumberAsString<u64>> },
}

impl From<VdafInstance> for VdafObject {
    fn from(vdaf: VdafInstance) -> Self {
        match vdaf {
            VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Count) => {
                VdafObject::Prio3Aes128Count
            }
            VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128CountVec { length }) => {
                VdafObject::Prio3Aes128CountVec {
                    length: NumberAsString(length),
                }
            }
            VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Sum { bits }) => {
                VdafObject::Prio3Aes128Sum {
                    bits: NumberAsString(bits),
                }
            }
            VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Histogram {
                buckets,
            }) => VdafObject::Prio3Aes128Histogram {
                buckets: buckets.iter().copied().map(NumberAsString).collect(),
            },
            _ => panic!("Unsupported VDAF: {:?}", vdaf),
        }
    }
}

impl From<VdafObject> for VdafInstance {
    fn from(vdaf: VdafObject) -> Self {
        match vdaf {
            VdafObject::Prio3Aes128Count => {
                VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Count)
            }
            VdafObject::Prio3Aes128CountVec { length } => {
                VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128CountVec {
                    length: length.0,
                })
            }
            VdafObject::Prio3Aes128Sum { bits } => {
                VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Sum { bits: bits.0 })
            }
            VdafObject::Prio3Aes128Histogram { buckets } => {
                VdafInstance::Real(janus_core::task::VdafInstance::Prio3Aes128Histogram {
                    buckets: buckets.iter().map(|value| value.0).collect(),
                })
            }
        }
    }
}

#[derive(Debug)]
pub struct BadRoleError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AggregatorRole {
    Leader,
    Helper,
}

impl TryFrom<Role> for AggregatorRole {
    type Error = BadRoleError;

    fn try_from(role: Role) -> Result<Self, Self::Error> {
        match role {
            Role::Collector => Err(BadRoleError),
            Role::Client => Err(BadRoleError),
            Role::Leader => Ok(AggregatorRole::Leader),
            Role::Helper => Ok(AggregatorRole::Helper),
        }
    }
}

impl From<AggregatorRole> for Role {
    fn from(role: AggregatorRole) -> Self {
        match role {
            AggregatorRole::Leader => Role::Leader,
            AggregatorRole::Helper => Role::Helper,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AggregatorAddTaskRequest {
    pub task_id: String, // in unpadded base64url
    pub leader: Url,
    pub helper: Url,
    pub vdaf: VdafObject,
    pub leader_authentication_token: String,
    #[serde(default)]
    pub collector_authentication_token: Option<String>,
    pub role: AggregatorRole,
    pub verify_key: String, // in unpadded base64url
    pub max_batch_query_count: u64,
    pub query_type: u8,
    pub min_batch_size: u64,
    pub max_batch_size: Option<u64>,
    pub time_precision: u64,           // in seconds
    pub collector_hpke_config: String, // in unpadded base64url
    pub task_expiration: u64,          // in seconds since the epoch
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddTaskResponse {
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl From<Task> for AggregatorAddTaskRequest {
    fn from(task: Task) -> Self {
        let (query_type, max_batch_size) = match task.query_type() {
            QueryType::TimeInterval => (TimeInterval::CODE as u8, None),
            QueryType::FixedSize { max_batch_size } => {
                (FixedSize::CODE as u8, Some(*max_batch_size))
            }
        };
        Self {
            task_id: base64::encode_config(task.id().as_ref(), URL_SAFE_NO_PAD),
            leader: task.aggregator_url(&Role::Leader).unwrap().clone(),
            helper: task.aggregator_url(&Role::Helper).unwrap().clone(),
            vdaf: task.vdaf().clone().into(),
            leader_authentication_token: String::from_utf8(
                task.primary_aggregator_auth_token().as_bytes().to_vec(),
            )
            .unwrap(),
            collector_authentication_token: if task.role() == &Role::Leader {
                Some(
                    String::from_utf8(task.primary_collector_auth_token().as_bytes().to_vec())
                        .unwrap(),
                )
            } else {
                None
            },
            role: (*task.role()).try_into().unwrap(),
            verify_key: base64::encode_config(
                task.vdaf_verify_keys().first().unwrap().as_ref(),
                URL_SAFE_NO_PAD,
            ),
            max_batch_query_count: task.max_batch_query_count(),
            query_type,
            min_batch_size: task.min_batch_size(),
            max_batch_size,
            time_precision: task.time_precision().as_seconds(),
            collector_hpke_config: base64::encode_config(
                &task.collector_hpke_config().get_encoded(),
                URL_SAFE_NO_PAD,
            ),
            task_expiration: task.task_expiration().as_seconds_since_epoch(),
        }
    }
}

pub fn install_tracing_subscriber() -> anyhow::Result<()> {
    let stdout_filter = EnvFilter::from_default_env();
    let layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_level(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .pretty();
    let subscriber = Registry::default().with(stdout_filter.and_then(layer));
    tracing::subscriber::set_global_default(subscriber)?;

    LogTracer::init()?;

    Ok(())
}

/// This registry lazily generates up to 256 HPKE key pairs, one with each possible
/// [`HpkeConfigId`].
#[derive(Default)]
pub struct HpkeConfigRegistry {
    keypairs: HashMap<HpkeConfigId, (HpkeConfig, HpkePrivateKey)>,
}

impl HpkeConfigRegistry {
    pub fn new() -> HpkeConfigRegistry {
        Default::default()
    }

    /// Get the keypair associated with a given ID.
    pub fn fetch_keypair(&mut self, id: HpkeConfigId) -> (HpkeConfig, HpkePrivateKey) {
        self.keypairs
            .entry(id)
            .or_insert_with(|| {
                generate_hpke_config_and_private_key(
                    id,
                    // These algorithms should be broadly compatible with other DAP implementations, since they
                    // are required by section 6 of draft-ietf-ppm-dap-02.
                    HpkeKemId::X25519HkdfSha256,
                    HpkeKdfId::HkdfSha256,
                    HpkeAeadId::Aes128Gcm,
                )
            })
            .clone()
    }

    /// Choose a random [`HpkeConfigId`], and then get the keypair associated with that ID.
    pub fn get_random_keypair(&mut self) -> (HpkeConfig, HpkePrivateKey) {
        self.fetch_keypair(random::<u8>().into())
    }
}

/// log_export_path returns the path to export container logs to, or None if container logs are not
/// configured to be exported.
///
/// The resulting value is based directly on the JANUS_E2E_LOGS_PATH environment variable.
pub fn log_export_path() -> Option<PathBuf> {
    match env::var("JANUS_E2E_LOGS_PATH") {
        Ok(logs_path) => Some(PathBuf::from_str(&logs_path).unwrap()),
        Err(VarError::NotPresent) => None,
        Err(err) => panic!("Failed to parse JANUS_E2E_LOGS_PATH: {err}"),
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use backoff::{future::retry, ExponentialBackoff};
    use futures::{Future, TryFutureExt};
    use rand::random;
    use std::{fmt::Debug, time::Duration};
    use url::Url;

    lazy_static::lazy_static! {
        static ref DOCKER_HASH_RE: regex::Regex = regex::Regex::new(r"sha256:([0-9a-f]{64})").unwrap();
    }

    async fn await_readiness_condition<
        I,
        E: Debug,
        Fn: FnMut() -> Fut,
        Fut: Future<Output = Result<I, backoff::Error<E>>>,
    >(
        operation: Fn,
    ) {
        retry(
            // (We use ExponentialBackoff as a constant-time backoff as the built-in Constant
            // backoff will never time out.)
            ExponentialBackoff {
                initial_interval: Duration::from_millis(250),
                max_interval: Duration::from_millis(250),
                multiplier: 1.0,
                max_elapsed_time: Some(Duration::from_secs(30)),
                ..Default::default()
            },
            operation,
        )
        .await
        .unwrap();
    }

    /// Waits a while for the given IPv4 port to start responding successfully to
    /// "/internal/test/ready" HTTP requests, panicking if this doesn't happen soon enough.
    pub async fn await_ready_ok(port: u16) {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        // Explicitly connect to IPv4 localhost so that we don't accidentally try to talk to a
        // different container's IPv6 port.
        let url = Url::parse(&format!("http://127.0.0.1:{port}/internal/test/ready")).unwrap();
        await_readiness_condition(|| async {
            http_client
                .post(url.clone())
                .send()
                .await
                .map_err(backoff::Error::transient)?
                .error_for_status()
                .map_err(backoff::Error::transient)
        })
        .await
    }

    /// Waits a while for the given port to start responding to HTTP requests, panicking if this
    /// doesn't happen soon enough.
    pub async fn await_http_server(port: u16) {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        await_readiness_condition(|| {
            http_client
                .get(url.clone())
                .send()
                .map_err(backoff::Error::transient)
        })
        .await
    }

    /// Loads a given zstd-compressed docker image into Docker. Returns the hash of the loaded
    /// image, e.g. as referenced by `sha256:$HASH`. Panics on failure.
    pub fn load_zstd_compressed_docker_image(compressed_image: &[u8]) -> String {
        use std::{
            io::{Cursor, Read},
            process::{Command, Stdio},
            thread,
        };

        let mut docker_load_child = Command::new("docker")
            .args(["load", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to execute `docker load`");
        let child_stdin = docker_load_child.stdin.take().unwrap();
        thread::scope(|s| {
            let writer_handle = s.spawn(|| {
                // We write in a separate thread as "writing more than a pipe buffer's
                // worth of input to stdin without also reading stdout and stderr at the
                // same time may cause a deadlock."
                zstd::stream::copy_decode(Cursor::new(compressed_image), child_stdin)
            });
            let reader_handle = s.spawn(|| {
                let mut child_stdout = docker_load_child.stdout.take().unwrap();
                let mut stdout = String::new();
                child_stdout
                    .read_to_string(&mut stdout)
                    .expect("Couldn't read image ID from docker");
                let caps = DOCKER_HASH_RE
                    .captures(&stdout)
                    .expect("Couldn't find image ID from `docker load` output");
                caps.get(1).unwrap().as_str().to_string()
            });

            // The first `expect` catches panics, the second `expect` catches write errors.
            writer_handle
                .join()
                .expect("Couldn't write image to docker")
                .expect("Couldn't write image to docker");
            reader_handle.join().unwrap()
        })
    }

    pub fn generate_network_name() -> String {
        generate_unique_name("janus_ephemeral_network")
    }

    pub fn generate_unique_name(prefix: &str) -> String {
        format!("{}_{}", prefix, hex::encode(random::<[u8; 4]>()))
    }
}
