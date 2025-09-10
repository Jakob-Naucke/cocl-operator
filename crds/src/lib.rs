// SPDX-FileCopyrightText: Alice Frosi <afrosi@redhat.com>
// SPDX-FileCopyrightText: Jakob Naucke <jnaucke@redhat.com>
//
// SPDX-License-Identifier: MIT

use kube_derive::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod mc;
mod mcp;

pub use mc::MachineConfig;
pub use mcp::MachineConfigPool;

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "confidential-containers.io",
    version = "v1alpha1",
    kind = "ConfidentialCluster",
    namespaced,
    plural = "confidentialclusters"
)]
#[serde(rename_all = "camelCase")]
pub struct ConfidentialClusterSpec {
    pub trustee_image: String,
    pub pcrs_compute_image: String,
    pub register_server_image: String,
    pub trustee_addr: String,
    pub register_server_port: i32,
}

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "confidential-containers.io",
    version = "v1alpha1",
    kind = "Machine",
    namespaced,
    plural = "machines"
)]
pub struct MachineSpec {
    pub id: String,
    pub address: String,
}

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "confidential-containers.io",
    version = "v1alpha1",
    kind = "ApprovedImage",
    namespaced,
    plural = "approvedimages"
)]
pub struct ApprovedImageSpec {
    pub reference: String,
}
