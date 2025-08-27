use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use clap::Parser;
use compute_pcrs_lib::*;
use json_patch::{AddOperation, PatchOperation, TestOperation};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use log::info;
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::json;
use std::cmp;
use std::collections::{BTreeMap, BTreeSet};

const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
const REFERENCE_FILENAME: &str = "reference-values.json";

fn primitive_date_time_to_str<S>(d: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&d.format(TIME_FORMAT).to_string())
}

fn primitive_date_time_from_str<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<DateTime<Utc>, D::Error> {
    let s = <Option<&str>>::deserialize(d)?
        .ok_or_else(|| serde::de::Error::invalid_length(0, &"<TIME>"))?;

    let ndt = NaiveDateTime::parse_from_str(s, TIME_FORMAT)
        .map_err(|err| serde::de::Error::custom::<String>(err.to_string()))?;

    Ok(DateTime::from_naive_utc_and_offset(ndt, Utc))
}

/// Sync with Trustee
/// reference_value_provider_service::reference_value::ReferenceValue
#[derive(Deserialize, Serialize)]
struct ReferenceValue {
    pub version: Version,
    pub name: String,
    #[serde(
        serialize_with = "primitive_date_time_to_str",
        deserialize_with = "primitive_date_time_from_str"
    )]
    pub expiration: DateTime<Utc>,
    pub value: Vec<String>,
}

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path to the kernel modules directory
    #[arg(short, long)]
    kernels: String,
    /// Path to the ESP directory
    #[arg(short, long)]
    esp: String,
    /// Path to the directory storing EFIVar files
    #[arg(short = 's', long)]
    efivars: String,
    /// Path to directory storing MokListRT, MokListTrustedRT and MokListXRT
    #[arg(short, long)]
    mokvars: String,
    /// ConfigMap name to write to
    #[arg(short, long)]
    configmap: String,
    /// Namespace to write ConfigMap to
    #[arg(short, long)]
    namespace: String,
}

struct RvContent {
    version: Version,
    expiration: DateTime<Utc>,
    values: BTreeSet<String>,
}

struct ReferenceValues(Vec<ReferenceValue>);

impl ReferenceValues {
    fn to_map(&self) -> BTreeMap<String, RvContent> {
        let mut out = BTreeMap::new();
        for rv in self.0.iter() {
            let content = RvContent {
                version: rv.version.clone(),
                expiration: rv.expiration,
                values: rv.value.iter().cloned().collect(),
            };
            out.insert(rv.name.clone(), content);
        }
        out
    }

    fn merge(&self, other: &ReferenceValues) -> ReferenceValues {
        let join_by = |a: &mut RvContent, b: &RvContent| {
            a.version = cmp::max(a.version.clone(), b.version.clone());
            a.expiration = cmp::max(a.expiration, b.expiration);
            a.values = a.values.union(&b.values).cloned().collect();
        };
        let mut self_map = self.to_map();
        for (k2, v2) in other.to_map() {
            self_map
                .entry(k2)
                .and_modify(|v1| join_by(v1, &v2))
                .or_insert(v2);
        }
        let out = self_map
            .iter()
            .map(|(name, content)| ReferenceValue {
                version: content.version.clone(),
                name: name.to_string(),
                expiration: content.expiration,
                value: content.values.iter().cloned().collect(),
            })
            .collect();
        ReferenceValues(out)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut pcrs: Vec<_> = [
        compute_pcr4(&args.kernels, &args.esp, false, true),
        compute_pcr7(Some(&args.efivars), &args.esp, true),
        compute_pcr14(&args.mokvars),
    ]
    .iter()
    .map(|pcr| (format!("pcr{}", pcr.id), pcr.value.clone()))
    .collect();
    pcrs.push(("svn".to_string(), "1".to_string()));

    let version = Version::parse("0.1.0").unwrap();
    let reference_values: Vec<_> = pcrs
        .iter()
        .map(|(name, value)| ReferenceValue {
            version: version.clone(),
            name: format!("tpm_{name}"),
            expiration: Utc::now() + TimeDelta::days(365),
            value: vec![value.to_string()],
        })
        .collect();

    let client = Client::try_default().await?;
    let config_maps: Api<ConfigMap> = Api::namespaced(client, &args.namespace);

    let existing_rvs_json = config_maps.get(&args.configmap).await?.data;
    let existing_rvs: Vec<_> = existing_rvs_json
        .as_ref()
        .and_then(|m| m.get(REFERENCE_FILENAME))
        .and_then(|s| serde_json::from_str::<Vec<ReferenceValue>>(s).ok())
        .unwrap_or_default();
    let merged = ReferenceValues(existing_rvs).merge(&ReferenceValues(reference_values));

    let reference_values_json = serde_json::to_string(&merged.0)?;
    let data = BTreeMap::from([(
        REFERENCE_FILENAME.to_string(),
        reference_values_json.to_string(),
    )]);

    let path = jsonptr::PointerBuf::parse("/data")?;
    let mut patches = Vec::new();
    if let Some(map) = existing_rvs_json {
        patches.push(PatchOperation::Test(TestOperation {
            path: path.clone(),
            value: json!(map),
        }));
    }
    patches.push(PatchOperation::Add(AddOperation {
        path,
        value: json!(data),
    }));

    let json_patch = json_patch::Patch(patches);
    let patch: Patch<ConfigMap> = Patch::Json(json_patch);
    let params = PatchParams::default();

    config_maps.patch(&args.configmap, &params, &patch).await?;
    info!("Patch ConfigMap {}", args.configmap);
    Ok(())
}
