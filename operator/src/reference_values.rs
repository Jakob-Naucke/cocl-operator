// SPDX-FileCopyrightText: Alice Frosi <afrosi@redhat.com>
// SPDX-FileCopyrightText: Jakob Naucke <jnaucke@redhat.com>
//
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use compute_pcrs_lib::Pcr;
use futures_util::{Stream, StreamExt, TryStreamExt};
use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{
        ConfigMap, ConfigMapVolumeSource, Container, ImageVolumeSource, KeyToPath, PodSpec,
        PodTemplateSpec, Volume, VolumeMount,
    },
};
use kube::api::{DeleteParams, ObjectList, ObjectMeta, PostParams, WatchEvent};
use kube::runtime::{
    controller::{Action, Controller},
    watcher,
};
use kube::{Api, Client};
use log::{error, info, warn};
use oci_client::secrets::RegistryAuth;
use oci_spec::image::ImageConfiguration;
use openssl::hash::{MessageDigest, hash};
use serde::Deserialize;
use std::{
    boxed::Box, collections::BTreeMap, marker::Send, path::PathBuf, pin::Pin, sync::Arc,
    time::Duration,
};

use crate::trustee::{self, get_image_pcrs};
use crds::{ApprovedImage, ApprovedImageSpec, MachineConfig, MachineConfigPool};
use operator::{
    ControllerError, RvContextData, controller_error_policy, controller_info, info_if_exists,
};
use rv_store::*;

const JOB_LABEL_KEY: &str = "kind";
const PCR_COMMAND_NAME: &str = "compute-pcrs";
const PCR_LABEL: &str = "org.coreos.pcrs";

/// Synchronize with compute_pcrs_cli::Output
#[derive(Deserialize)]
struct ComputePcrsOutput {
    pcrs: Vec<Pcr>,
}

/// Format container image name as RFC1035 (minus truncation) and give unique name
fn image_rfc1035(image: &str) -> Result<String> {
    // Hash for images that only differed beyond the truncation limit or in replaced characters
    let replaced = image.replace(['.', ':', '/', '@', '_'], "-");
    let hash = hash(MessageDigest::sha1(), image.as_bytes())?;
    let hash_str = hex::encode(hash)[..10].to_string();
    Ok(format!("{hash_str}-{replaced}"))
}

pub async fn create_pcrs_config_map(client: Client) -> Result<()> {
    let empty_data = BTreeMap::from([(
        PCR_CONFIG_FILE.to_string(),
        serde_json::to_string(&ImagePcrs::default())?,
    )]);
    let config_maps: Api<ConfigMap> = Api::default_namespaced(client);
    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(PCR_CONFIG_MAP.to_string()),
            ..Default::default()
        },
        data: Some(empty_data),
        ..Default::default()
    };
    let create = config_maps
        .create(&PostParams::default(), &config_map)
        .await;
    info_if_exists!(create, "ConfigMap", PCR_CONFIG_MAP);

    Ok(())
}

async fn fetch_pcr_label(image_ref: &str) -> Result<Option<Vec<Pcr>>> {
    let reference: oci_client::Reference = image_ref.parse()?;
    let client = oci_client::Client::new(Default::default());
    let (_, _, raw_config) = client
        .pull_manifest_and_config(&reference, &RegistryAuth::Anonymous)
        .await?;
    let config: ImageConfiguration = serde_json::from_str(&raw_config)?;
    config
        .labels_of_config()
        .and_then(|m| m.get(PCR_LABEL))
        .map(|l| serde_json::from_str::<ComputePcrsOutput>(l).map(|o| o.pcrs))
        .transpose()
        .map_err(Into::into)
}

fn build_compute_pcrs_pod_spec(boot_image: &str, pcrs_compute_image: &str) -> PodSpec {
    let image_volume_name = "image";
    let image_mountpoint = PathBuf::from(format!("/{image_volume_name}"));
    let pcrs_volume_name = "pcrs";
    let pcrs_mountpoint = PathBuf::from(format!("/{pcrs_volume_name}"));

    let mut cmd = vec![PCR_COMMAND_NAME.to_string()];
    let mut add_flag = |flag: &str, value: &str| {
        cmd.push(format!("--{flag}"));
        cmd.push(value.to_string());
    };
    for (flag, path_suffix) in [
        ("kernels", "usr/lib/modules"),
        ("esp", "usr/lib/bootupd/updates"),
    ] {
        let full_path = image_mountpoint.clone().join(path_suffix);
        add_flag(flag, full_path.to_str().unwrap());
    }
    for (flag, value) in [
        ("efivars", "/reference-values/efivars/qemu-ovmf/fedora-42"),
        ("mokvars", "/reference-values/mok-variables/fedora-42"),
        ("image", boot_image),
    ] {
        add_flag(flag, value);
    }

    PodSpec {
        service_account_name: Some("compute-pcrs".to_string()),
        containers: vec![Container {
            name: PCR_COMMAND_NAME.to_string(),
            image: Some(pcrs_compute_image.to_string()),
            command: Some(cmd),
            volume_mounts: Some(vec![
                VolumeMount {
                    name: image_volume_name.to_string(),
                    mount_path: image_mountpoint.to_str().unwrap().to_string(),
                    ..Default::default()
                },
                VolumeMount {
                    name: pcrs_volume_name.to_string(),
                    mount_path: pcrs_mountpoint.to_str().unwrap().to_string(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }],
        volumes: Some(vec![
            Volume {
                name: image_volume_name.to_string(),
                image: Some(ImageVolumeSource {
                    reference: Some(boot_image.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Volume {
                name: pcrs_volume_name.to_string(),
                config_map: Some(ConfigMapVolumeSource {
                    name: PCR_CONFIG_MAP.to_string(),
                    items: Some(vec![KeyToPath {
                        key: PCR_CONFIG_FILE.to_string(),
                        path: PCR_CONFIG_FILE.to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        restart_policy: Some("Never".to_string()),
        ..Default::default()
    }
}

async fn handle_mcp(mcp: Arc<MachineConfigPool>, client: Arc<Client>) -> anyhow::Result<()> {
    let err = "MCP changed, but had no name";
    let mcp_name = &mcp.metadata.name.clone().context(err)?;
    let mut err = format!("MCP {mcp_name} changed, but had no configuration");
    let config = &mcp.spec.configuration.clone().context(err)?;
    err = format!("MCP {mcp_name} changed, but config had no name");
    let config_name = &config.name.clone().context(err)?;

    let mcs: Api<MachineConfig> = Api::all(Arc::unwrap_or_clone(client.clone()));
    let mc = mcs.get(config_name).await?;
    err = format!("MC {config_name} existed, but had no osImageUrl");
    let url = mc.spec.os_image_url.context(err)?;

    let image = ApprovedImage {
        metadata: ObjectMeta {
            name: Some(image_rfc1035(&url)?),
            ..Default::default()
        },
        spec: ApprovedImageSpec { reference: url },
    };
    let images: Api<ApprovedImage> = Api::default_namespaced(Arc::unwrap_or_clone(client));
    images.create(&Default::default(), &image).await?;
    Ok(())
}

async fn mc_reconcile(
    mcp: Arc<MachineConfigPool>,
    client: Arc<Client>,
) -> Result<Action, ControllerError> {
    handle_mcp(mcp, client).await?;
    Ok(Action::await_change())
}

pub async fn populate_initial_rvs(client: Client, mcps: ObjectList<MachineConfigPool>) {
    for mcp in mcps {
        let name = operator::name_or_default(&mcp.metadata);
        match handle_mcp(Arc::new(mcp), Arc::new(client.clone())).await {
            Ok(_) => info!("Set image of existing MCP {name} as approved"),
            Err(e) => info!("Failed to set image of existing MCP {name}: {e}"),
        }
    }
}

pub async fn launch_rv_mc_controller(client: Client) {
    let mcps: Api<MachineConfigPool> = Api::all(client.clone());
    tokio::spawn(
        Controller::new(mcps, watcher::Config::default())
            .run(mc_reconcile, controller_error_policy, Arc::new(client))
            .for_each(controller_info),
    );
}

async fn job_reconcile(job: Arc<Job>, ctx: Arc<RvContextData>) -> Result<Action, ControllerError> {
    let err = "Job changed, but had no name";
    let name = &job.metadata.name.clone().context(err)?;
    let err = format!("Job {name} changed, but had no status");
    let status = &job.status.clone().context(err)?;
    if status.completion_time.is_none() {
        info!("Job {name} changed, but had not completed");
        return Ok(Action::requeue(Duration::from_secs(300)));
    }
    let jobs: Api<Job> = Api::default_namespaced(ctx.client.clone());
    // Foreground deletion: Delete the pod too
    let delete = jobs.delete(name, &DeleteParams::foreground()).await;
    delete.map_err(Into::<anyhow::Error>::into)?;
    trustee::update_reference_values(Arc::unwrap_or_clone(ctx)).await?;
    Ok(Action::await_change())
}

pub async fn launch_rv_job_controller(ctx: RvContextData) {
    let jobs: Api<Job> = Api::default_namespaced(ctx.client.clone());
    let watcher = watcher::Config {
        label_selector: Some(format!("{JOB_LABEL_KEY}={PCR_COMMAND_NAME}")),
        ..Default::default()
    };
    tokio::spawn(
        Controller::new(jobs, watcher)
            .run(job_reconcile, controller_error_policy, Arc::new(ctx))
            .for_each(controller_info),
    );
}

async fn compute_fresh_pcrs(ctx: RvContextData, boot_image: &str) -> Result<()> {
    let rfc1035_image_name = image_rfc1035(boot_image)?;
    let mut job_name = format!("{PCR_COMMAND_NAME}-{rfc1035_image_name}");
    job_name.truncate(63);

    let pod_spec = build_compute_pcrs_pod_spec(boot_image, &ctx.pcrs_compute_image);
    let job = Job {
        metadata: ObjectMeta {
            name: Some(job_name.clone()),
            labels: Some(BTreeMap::from([(
                JOB_LABEL_KEY.to_string(),
                PCR_COMMAND_NAME.to_string(),
            )])),
            owner_references: Some(vec![ctx.owner_reference]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            template: PodTemplateSpec {
                spec: Some(pod_spec),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let jobs: Api<Job> = Api::default_namespaced(ctx.client);
    let create = jobs.create(&PostParams::default(), &job).await;
    info_if_exists!(create, "Job", job_name);
    Ok(())
}

async fn handle_new_image(ctx: RvContextData, boot_image: &str) -> Result<()> {
    let config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client.clone());
    let mut image_pcrs_map = config_maps.get(PCR_CONFIG_MAP).await?;
    let mut image_pcrs = get_image_pcrs(image_pcrs_map.clone())?;
    if image_pcrs.0.contains_key(boot_image) {
        return Ok(());
    }
    let label = fetch_pcr_label(boot_image).await?;
    if label.is_none() {
        return compute_fresh_pcrs(ctx, boot_image).await;
    }

    // Non-goal: Support tags whose referenced versions change (e.g. `latest`).
    // This would introduce hard-to-define behavior for disallowing older versions that were
    // introduced as a tag that is still allowed.
    image_pcrs.0.insert(boot_image.to_string(), label.unwrap());
    let image_pcrs_json = serde_json::to_string(&image_pcrs)?;
    let data = BTreeMap::from([(PCR_CONFIG_FILE.to_string(), image_pcrs_json.to_string())]);
    image_pcrs_map.data = Some(data);
    config_maps
        .replace(PCR_CONFIG_MAP, &PostParams::default(), &image_pcrs_map)
        .await?;
    trustee::update_reference_values(ctx).await
}

async fn disallow_image(ctx: RvContextData, boot_image: &str) -> Result<()> {
    let config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client.clone());
    let mut image_pcrs_map = config_maps.get(PCR_CONFIG_MAP).await?;
    let mut image_pcrs = get_image_pcrs(image_pcrs_map.clone())?;
    if image_pcrs.0.remove(boot_image).is_none() {
        info!("Image {boot_image} was to be disallowed, but already was not allowed");
    }

    let image_pcrs_json = serde_json::to_string(&image_pcrs)?;
    let data = BTreeMap::from([(PCR_CONFIG_FILE.to_string(), image_pcrs_json.to_string())]);
    image_pcrs_map.data = Some(data);
    config_maps
        .replace(PCR_CONFIG_MAP, &PostParams::default(), &image_pcrs_map)
        .await?;
    trustee::update_reference_values(ctx).await
}

pub async fn watch_images(
    mut stream: Pin<
        Box<dyn Stream<Item = std::result::Result<WatchEvent<ApprovedImage>, kube::Error>> + Send>,
    >,
    ctx: RvContextData,
) {
    loop {
        match stream.try_next().await {
            Ok(Some(WatchEvent::Added(s))) | Ok(Some(WatchEvent::Modified(s))) => {
                match handle_new_image(ctx.clone(), &s.spec.reference).await {
                    Ok(_) => info!("Added PCRs for image {}", s.spec.reference),
                    Err(e) => error!("Failed to add PCRs for image {}: {e}", s.spec.reference),
                }
            }
            Ok(Some(WatchEvent::Deleted(s))) => {
                match disallow_image(ctx.clone(), &s.spec.reference).await {
                    Ok(_) => info!("Disallowed image {}", s.spec.reference),
                    Err(e) => error!("Failed to disallow image {}: {e}", s.spec.reference),
                }
            }
            Ok(Some(WatchEvent::Error(e))) => warn!("Error watching ApprovedImages: {e}"),
            Ok(Some(_)) => {}
            Err(e) => warn!("Error receiving ApprovedImages event: {e}"),
            Ok(None) => return,
        }
    }
}
