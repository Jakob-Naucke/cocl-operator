// SPDX-FileCopyrightText: Alice Frosi <afrosi@redhat.com>
// SPDX-FileCopyrightText: Jakob Naucke <jnaucke@redhat.com>
//
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use compute_pcrs_lib::Pcr;
use futures_util::StreamExt;
use k8s_openapi::{
    api::{
        batch::v1::{Job, JobSpec},
        core::v1::{
            ConfigMap, ConfigMapVolumeSource, Container, ImageVolumeSource, KeyToPath, PodSpec,
            PodTemplateSpec, Volume, VolumeMount,
        },
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::api::{DeleteParams, ObjectMeta};
use kube::runtime::{
    controller::{Action, Controller},
    finalizer,
    finalizer::Event,
    watcher,
};
use kube::{Api, Client, Resource};
use log::{info, warn};
use oci_client::secrets::RegistryAuth;
use oci_spec::image::ImageConfiguration;
use openssl::hash::{MessageDigest, hash};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use crate::trustee::{self, get_image_pcrs};
use cocl_operator_lib::{conditions::*, reference_values::*, *};
use operator::*;

const JOB_LABEL_KEY: &str = "kind";
const PCR_COMMAND_NAME: &str = "compute-pcrs";
const PCR_LABEL: &str = "org.coreos.pcrs";
const APPROVED_IMAGE_FINALIZER: &str = "finalizer.approved-image.confidential-clusters.io";

/// Synchronize with compute_pcrs_cli::Output
#[derive(Deserialize)]
struct ComputePcrsOutput {
    pcrs: Vec<Pcr>,
}

pub async fn create_pcrs_config_map(client: Client, owner_reference: OwnerReference) -> Result<()> {
    let empty_data = BTreeMap::from([(
        PCR_CONFIG_FILE.to_string(),
        serde_json::to_string(&ImagePcrs::default())?,
    )]);
    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(PCR_CONFIG_MAP.to_string()),
            owner_references: Some(vec![owner_reference]),
            ..Default::default()
        },
        data: Some(empty_data),
        ..Default::default()
    };
    create_or_info_if_exists!(client, ConfigMap, config_map);
    Ok(())
}

async fn fetch_pcr_label(image_ref: &oci_client::Reference) -> Result<Option<Vec<Pcr>>> {
    let client = oci_client::Client::new(Default::default());
    let (_, _, raw_config) = client
        .pull_manifest_and_config(image_ref, &RegistryAuth::Anonymous)
        .await?;
    let config: ImageConfiguration = serde_json::from_str(&raw_config)?;
    config
        .labels_of_config()
        .and_then(|m| m.get(PCR_LABEL))
        .map(|l| serde_json::from_str::<ComputePcrsOutput>(l).map(|o| o.pcrs))
        .transpose()
        .map_err(Into::into)
}

fn build_compute_pcrs_pod_spec(
    resource_name: &str,
    boot_image: &str,
    pcrs_compute_image: &str,
) -> PodSpec {
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
        ("resource-name", resource_name),
    ] {
        add_flag(flag, value);
    }

    PodSpec {
        service_account_name: Some("cocl-operator".to_string()),
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

// Name job by sanitized image name, plus a hash to disambiguate
// tags that differed only beyond the truncation limit
fn get_job_name(boot_image: &str) -> Result<String> {
    let rfc1035_boot_image = boot_image.replace(['.', ':', '/', '@', '_'], "-");
    let boot_image_hash = hash(MessageDigest::sha1(), boot_image.as_bytes())?;
    let mut boot_image_hash_str = hex::encode(boot_image_hash);
    boot_image_hash_str.truncate(10);
    let job_name = format!("{PCR_COMMAND_NAME}-{boot_image_hash_str}-{rfc1035_boot_image}");
    let trimmed: String = job_name.chars().take(63).collect();
    let trimmed = trimmed.trim_end_matches('-').to_string();
    Ok(trimmed)
}

async fn compute_fresh_pcrs(
    ctx: RvContextData,
    resource_name: &str,
    boot_image: &str,
) -> anyhow::Result<()> {
    let job_name = get_job_name(boot_image)?;
    let pod_spec = build_compute_pcrs_pod_spec(resource_name, boot_image, &ctx.pcrs_compute_image);
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
    create_or_info_if_exists!(ctx.client, Job, job);
    Ok(())
}

async fn image_reconcile(
    image: Arc<ApprovedImage>,
    ctx: Arc<RvContextData>,
) -> Result<Action, ControllerError> {
    let kube_client = ctx.client.clone();
    let err = "ApprovedImage had no name";
    let name = image.metadata.name.clone().expect(err);

    let images: Api<ApprovedImage> = Api::default_namespaced(kube_client);
    let finalizer_ctx = Arc::unwrap_or_clone(ctx);
    finalizer(&images, APPROVED_IMAGE_FINALIZER, image, |ev| async {
        match ev {
            Event::Apply(image) => image_add_reconcile(finalizer_ctx, &image).await,
            Event::Cleanup(_) => disallow_image(finalizer_ctx, &name)
                .await
                .map(|_| Action::await_change())
                .map_err(|e| finalizer::Error::<ControllerError>::CleanupFailed(e.into())),
        }
    })
    .await
    .map_err(|e| anyhow!("failed to reconcile on image: {e}").into())
}

async fn image_add_reconcile(
    ctx: RvContextData,
    image: &ApprovedImage,
) -> Result<Action, finalizer::Error<ControllerError>> {
    let kube_client = ctx.client.clone();
    let name = image.metadata.name.as_ref().unwrap();
    let (action, reason) = match handle_new_image(ctx, name, &image.spec.reference).await {
        Ok(reason) => (Action::await_change(), reason),
        Err(e) => {
            warn!("PCR computation for {name} failed: {e}");
            let action = Action::requeue(Duration::from_secs(60));
            (action, NOT_COMMITTED_REASON_FAILED)
        }
    };
    let committed = committed_condition(reason, image.metadata.generation);
    let conditions = Some(vec![committed]);
    let images: Api<ApprovedImage> = Api::default_namespaced(kube_client);
    update_status!(images, &name, ApprovedImageStatus { conditions })
        .map_err(|e| finalizer::Error::<ControllerError>::ApplyFailed(e.into()))?;
    Ok(action)
}

pub async fn launch_rv_image_controller(ctx: RvContextData) {
    let images: Api<ApprovedImage> = Api::default_namespaced(ctx.client.clone());
    tokio::spawn(
        Controller::new(images, Default::default())
            .run(image_reconcile, controller_error_policy, Arc::new(ctx))
            .for_each(controller_info),
    );
}

pub async fn handle_new_image(
    ctx: RvContextData,
    resource_name: &str,
    boot_image: &str,
) -> Result<&'static str> {
    let config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client.clone());
    let mut image_pcrs_map = config_maps.get(PCR_CONFIG_MAP).await?;
    let mut image_pcrs = get_image_pcrs(image_pcrs_map.clone())?;
    if let Some(pcr) = image_pcrs.0.get(resource_name) {
        if pcr.reference == boot_image {
            info!("Image {boot_image} was to be allowed, but already was allowed");
            return trustee::update_reference_values(ctx)
                .await
                .map(|_| COMMITTED_REASON);
        }
    }
    let image_ref: oci_client::Reference = boot_image.parse()?;
    if image_ref.digest().is_none() {
        warn!(
            "Image {boot_image} did not specify a digest. \
             Only images with a digest are supported to avoid ambiguity."
        );
        return Ok(NOT_COMMITTED_REASON_NO_DIGEST);
    }
    let label = fetch_pcr_label(&image_ref).await?;
    if label.is_none() {
        return compute_fresh_pcrs(ctx, resource_name, boot_image)
            .await
            .map(|_| NOT_COMMITTED_REASON_COMPUTING);
    }

    let image_pcr = ImagePcr {
        first_seen: Utc::now(),
        pcrs: label.unwrap(),
        reference: boot_image.to_string(),
    };
    image_pcrs.0.insert(resource_name.to_string(), image_pcr);
    let image_pcrs_json = serde_json::to_string(&image_pcrs)?;
    let data = BTreeMap::from([(PCR_CONFIG_FILE.to_string(), image_pcrs_json.to_string())]);
    image_pcrs_map.data = Some(data);
    config_maps
        .replace(PCR_CONFIG_MAP, &Default::default(), &image_pcrs_map)
        .await?;
    trustee::update_reference_values(ctx)
        .await
        .map(|_| COMMITTED_REASON)
}

pub async fn disallow_image(ctx: RvContextData, resource_name: &str) -> Result<()> {
    let config_maps: Api<ConfigMap> = Api::default_namespaced(ctx.client.clone());
    let mut image_pcrs_map = config_maps.get(PCR_CONFIG_MAP).await?;
    let mut image_pcrs = get_image_pcrs(image_pcrs_map.clone())?;
    if image_pcrs.0.remove(resource_name).is_none() {
        info!("Image {resource_name} was to be disallowed, but already was not allowed");
    }

    let image_pcrs_json = serde_json::to_string(&image_pcrs)?;
    let data = BTreeMap::from([(PCR_CONFIG_FILE.to_string(), image_pcrs_json.to_string())]);
    image_pcrs_map.data = Some(data);
    config_maps
        .replace(PCR_CONFIG_MAP, &Default::default(), &image_pcrs_map)
        .await?;
    trustee::update_reference_values(ctx).await
}
