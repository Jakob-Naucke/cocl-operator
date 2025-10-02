// SPDX-FileCopyrightText: Alice Frosi <afrosi@redhat.com>
// SPDX-FileCopyrightText: Jakob Naucke <jnaucke@redhat.com>
//
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use chrono::{DateTime, TimeDelta, Utc};
use clevis_pin_trustee_lib::Key as ClevisKey;
use json_patch::{AddOperation, PatchOperation, TestOperation};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, PodSpec,
    PodTemplateSpec, Secret, SecretVolumeSource, Service, ServicePort, ServiceSpec, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::{
    apis::meta::v1::{LabelSelector, OwnerReference},
    util::intstr::IntOrString,
};
use kube::api::{ObjectMeta, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use log::info;
use operator::{RvContextData, info_if_exists};
use rv_store::*;
use serde::{Serialize, Serializer};
use serde_json::Value::String as JsonString;
use std::collections::BTreeMap;

const TRUSTEE_DATA_DIR: &str = "/opt/trustee";
const TRUSTEE_SECRETS_PATH: &str = "/opt/trustee/kbs-repository/default";
const KBS_CONFIG_FILE: &str = "kbs-config.toml";
const REFERENCE_VALUES_FILE: &str = "reference-values.json";

const TRUSTEE_DATA_MAP: &str = "trustee-data";
const ATT_POLICY_MAP: &str = "attestation-policy";
const DEPLOYMENT_NAME: &str = "trustee-deployment";
const INTERNAL_KBS_PORT: i32 = 8080;

fn primitive_date_time_to_str<S>(d: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Sync with Trustee
/// reference_value_provider_service::reference_value::ReferenceValue
/// (cannot import directly because its expiration doesn't serialize
/// right)
#[derive(Serialize)]
struct ReferenceValue {
    pub version: String,
    pub name: String,
    #[serde(serialize_with = "primitive_date_time_to_str")]
    pub expiration: DateTime<Utc>,
    pub value: serde_json::Value,
}

pub fn get_image_pcrs(image_pcrs_map: ConfigMap) -> Result<ImagePcrs> {
    let image_pcrs_data = image_pcrs_map
        .data
        .context("Image PCRs map existed, but had no data")?;
    let image_pcrs_str = image_pcrs_data
        .get(PCR_CONFIG_FILE)
        .context("Image PCRs data existed, but had no file")?;
    serde_json::from_str(image_pcrs_str).map_err(Into::into)
}

fn recompute_reference_values(image_pcrs: ImagePcrs) -> Vec<ReferenceValue> {
    // TODO many grub+shim:many OS image recompute once supported
    let mut reference_values_in =
        BTreeMap::from([("svn".to_string(), vec![JsonString("1".to_string())])]);
    for pcr in image_pcrs.0.values().flatten() {
        reference_values_in
            .entry(format!("pcr{}", pcr.id))
            .or_default()
            .push(JsonString(pcr.value.clone()));
    }
    reference_values_in
        .iter()
        .map(|(name, values)| ReferenceValue {
            version: "0.1.0".to_string(),
            name: format!("tpm_{name}"),
            expiration: Utc::now() + TimeDelta::days(365),
            value: serde_json::Value::Array(values.to_vec()),
        })
        .collect()
}

pub async fn update_reference_values(ctx: RvContextData) -> Result<()> {
    let operator_config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client.clone());
    let image_pcrs_map = operator_config_maps.get(PCR_CONFIG_MAP).await?;
    let reference_values = recompute_reference_values(get_image_pcrs(image_pcrs_map)?);

    let config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client);
    let existing_data = config_maps.get(TRUSTEE_DATA_MAP).await?;
    let err = format!("ConfigMap {TRUSTEE_DATA_MAP} existed, but had no data");
    let existing_data_map = existing_data.data.context(err)?;
    let err = format!("ConfigMap {TRUSTEE_DATA_MAP} existed, but had no reference values");
    let existing_rvs = existing_data_map.get(REFERENCE_VALUES_FILE).context(err)?;

    let path = jsonptr::PointerBuf::parse(format!("/data/{REFERENCE_VALUES_FILE}"))?;
    let test_patch = PatchOperation::Test(TestOperation {
        path: path.clone(),
        value: JsonString(existing_rvs.clone()),
    });
    let add_patch = PatchOperation::Add(AddOperation {
        path,
        value: JsonString(serde_json::to_string(&reference_values)?),
    });
    let patch: Patch<ConfigMap> = Patch::Json(json_patch::Patch(vec![test_patch, add_patch]));
    let params = PatchParams::default();
    config_maps.patch(TRUSTEE_DATA_MAP, &params, &patch).await?;
    info!("Recomputed reference values");
    Ok(())
}

fn generate_luks_key() -> Result<Vec<u8>> {
    // Constraint: 32 bytes b64-encoded, thus 24
    let mut pass = [0; 24];
    openssl::rand::rand_bytes(&mut pass)?;
    let key = general_purpose::STANDARD.encode(pass);
    let jwk = ClevisKey {
        key_type: "oct".to_string(),
        key,
    };
    serde_json::to_vec(&jwk).map_err(Into::into)
}

fn generate_secret_volume(id: &str) -> (Volume, VolumeMount) {
    (
        Volume {
            name: id.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(id.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        VolumeMount {
            name: id.to_string(),
            mount_path: format!("{TRUSTEE_SECRETS_PATH}/{id}"),
            ..Default::default()
        },
    )
}

async fn mount_secret(client: Client, id: &str) -> Result<()> {
    let deployments: Api<Deployment> = Api::default_namespaced(client);
    let mut deployment = deployments.get(DEPLOYMENT_NAME).await?;
    let err = format!("Deployment {DEPLOYMENT_NAME} existed, but had no spec");
    let depl_spec = deployment.spec.as_mut().context(err)?;
    let err = format!("Deployment {DEPLOYMENT_NAME} existed, but had no pod spec");
    let pod_spec = depl_spec.template.spec.as_mut().context(err)?;

    let (volume, volume_mount) = generate_secret_volume(id);
    pod_spec.volumes.get_or_insert_default().push(volume);
    let err = format!("Deployment {DEPLOYMENT_NAME} existed, but had no containers");
    let container = pod_spec.containers.get_mut(0).context(err)?;
    let vol_mounts = container.volume_mounts.get_or_insert_default();
    vol_mounts.push(volume_mount);

    deployments
        .replace(DEPLOYMENT_NAME, &PostParams::default(), &deployment)
        .await?;
    info!("Mounted secret {id} to {DEPLOYMENT_NAME}");
    Ok(())
}

pub async fn generate_secret(client: Client, id: &str) -> Result<()> {
    let secret_data = k8s_openapi::ByteString(generate_luks_key()?);
    let data = BTreeMap::from([("root".to_string(), secret_data)]);

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(id.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let secrets: Api<Secret> = Api::default_namespaced(client.clone());
    let create = secrets.create(&PostParams::default(), &secret).await;
    info_if_exists!(create, "Secret", id);
    mount_secret(client, id).await
}

pub async fn generate_attestation_policy(
    client: Client,
    owner_reference: OwnerReference,
) -> Result<()> {
    let policy_rego = include_str!("tpm.rego");
    let data = BTreeMap::from([
        ("default_cpu.rego".to_string(), policy_rego.to_string()),
        // Must create GPU policy or Trustee will attempt to write one to the read-only mount
        ("default_gpu.rego".to_string(), String::new()),
    ]);

    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(ATT_POLICY_MAP.to_string()),
            owner_references: Some(vec![owner_reference]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let config_maps: Api<ConfigMap> = Api::default_namespaced(client.clone());
    let params = &PostParams::default();
    let create = config_maps.create(params, &config_map).await;
    info_if_exists!(create, "ConfigMap", ATT_POLICY_MAP);

    Ok(())
}

pub async fn generate_trustee_data(client: Client, owner_reference: OwnerReference) -> Result<()> {
    let kbs_config = include_str!("kbs-config.toml");
    let policy_rego = include_str!("resource.rego");

    let data = BTreeMap::from([
        ("kbs-config.toml".to_string(), kbs_config.to_string()),
        ("policy.rego".to_string(), policy_rego.to_string()),
        (REFERENCE_VALUES_FILE.to_string(), "{}".to_string()),
    ]);

    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(TRUSTEE_DATA_MAP.to_string()),
            owner_references: Some(vec![owner_reference]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let config_maps: Api<ConfigMap> = Api::default_namespaced(client.clone());
    let params = &PostParams::default();
    let create = config_maps.create(params, &config_map).await;
    info_if_exists!(create, "ConfigMap", TRUSTEE_DATA_MAP);

    Ok(())
}

pub async fn generate_kbs_service(
    client: Client,
    owner_reference: OwnerReference,
    kbs_port: i32,
) -> Result<()> {
    let svc_name = "kbs-service";
    let selector = Some(BTreeMap::from([("app".to_string(), "kbs".to_string())]));

    let service = Service {
        metadata: ObjectMeta {
            name: Some(svc_name.to_string()),
            owner_references: Some(vec![owner_reference.clone()]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: selector.clone(),
            ports: Some(vec![ServicePort {
                name: Some("kbs-port".to_string()),
                port: kbs_port,
                target_port: Some(IntOrString::Int(INTERNAL_KBS_PORT)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let services: Api<Service> = Api::default_namespaced(client.clone());
    let create = services.create(&PostParams::default(), &service).await;
    info_if_exists!(create, "Service", svc_name);

    Ok(())
}

fn generate_kbs_volume_templates() -> [(&'static str, &'static str, Volume); 3] {
    [
        (
            ATT_POLICY_MAP,
            "/opt/trustee/policies/opa",
            Volume {
                config_map: Some(ConfigMapVolumeSource {
                    name: ATT_POLICY_MAP.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
        (
            TRUSTEE_DATA_MAP,
            TRUSTEE_DATA_DIR,
            Volume {
                config_map: Some(ConfigMapVolumeSource {
                    name: TRUSTEE_DATA_MAP.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
        (
            "resource-dir",
            TRUSTEE_SECRETS_PATH,
            Volume {
                empty_dir: Some(EmptyDirVolumeSource {
                    medium: Some("Memory".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
    ]
}

fn generate_kbs_pod_spec(image: &str) -> PodSpec {
    let volumes = generate_kbs_volume_templates();
    PodSpec {
        containers: vec![Container {
            command: Some(vec![
                "/usr/local/bin/kbs".to_string(),
                "--config-file".to_string(),
                format!("{TRUSTEE_DATA_DIR}/{KBS_CONFIG_FILE}"),
            ]),
            image: Some(image.to_string()),
            name: "kbs".to_string(),
            ports: Some(vec![ContainerPort {
                container_port: INTERNAL_KBS_PORT,
                ..Default::default()
            }]),
            volume_mounts: Some(
                volumes
                    .iter()
                    .map(|(name, mount_path, _)| VolumeMount {
                        name: name.to_string(),
                        mount_path: mount_path.to_string(),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }],
        volumes: Some(
            volumes
                .iter()
                .map(|(name, _, volume)| {
                    let mut volume = volume.clone();
                    volume.name = name.to_string();
                    volume.clone()
                })
                .collect(),
        ),
        ..Default::default()
    }
}

pub async fn generate_kbs_deployment(
    client: Client,
    owner_reference: OwnerReference,
    image: &str,
) -> Result<()> {
    let selector = Some(BTreeMap::from([("app".to_string(), "kbs".to_string())]));
    let pod_spec = generate_kbs_pod_spec(image);

    // Inspired by trustee-operator
    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(DEPLOYMENT_NAME.to_string()),
            owner_references: Some(vec![owner_reference]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            selector: LabelSelector {
                match_labels: selector.clone(),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: selector,
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let deployments: Api<Deployment> = Api::default_namespaced(client);
    let params = &PostParams::default();
    let create = deployments.create(params, &deployment).await;
    info_if_exists!(create, "Deployment", DEPLOYMENT_NAME);

    Ok(())
}
