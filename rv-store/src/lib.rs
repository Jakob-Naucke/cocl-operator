// SPDX-FileCopyrightText: Alice Frosi <afrosi@redhat.com>
// SPDX-FileCopyrightText: Jakob Naucke <jnaucke@redhat.com>
//
// SPDX-License-Identifier: MIT

use compute_pcrs_lib::Pcr;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PCR_CONFIG_MAP: &str = "image-pcrs";
pub const PCR_CONFIG_FILE: &str = "image-pcrs.json";

#[derive(Default, Deserialize, Serialize)]
pub struct ImagePcrs(pub BTreeMap<String, Vec<Pcr>>);
