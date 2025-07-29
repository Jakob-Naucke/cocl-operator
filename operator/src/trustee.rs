use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use crds::{KbsConfig, KbsConfigSpec, Trustee};
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::PostParams;
use kube::{Api, Client, Error};
use log::info;
use openssl::pkey::{PKey, Private};
use std::collections::BTreeMap;
use std::fs;

pub async fn generate_kbs_auth_public_key(
    client: Client,
    namespace: &str,
    secret_name: &str,
) -> anyhow::Result<()> {
    let keypair = PKey::generate_ed25519()?;

    let private_pem = keypair.private_key_to_pem_pkcs8()?;
    fs::write("privateKey", &private_pem)?;

    let public_key = keypair.public_key_to_pem()?;
    fs::write("publicKey", &public_key)?;

    let public_key_b64 = general_purpose::STANDARD.encode(&public_key);

    let mut data = BTreeMap::new();
    data.insert(
        "publicKey".to_string(),
        k8s_openapi::ByteString(public_key_b64.into()),
    );

    let secret = Secret {
        metadata: kube::api::ObjectMeta {
            name: Some(secret_name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let secrets: Api<Secret> = Api::namespaced(client, namespace);
    match secrets.create(&PostParams::default(), &secret).await {
        Ok(s) => info!("Create secret {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => info!("Secret {} already exists", secret_name),
        Err(e) => return Err(e.into()),
    }

    Ok(())
}


pub async fn generate_kbs_https_certificate(namespace: &str, secret_name: &str) -> anyhow::Result<()> {
    todo!();
}

pub async fn generate_kbs_configuration(
    client: Client,
    namespace: &str,
    name: &str,
) -> anyhow::Result<()> {
    let kbs_config_toml = r#"[http_server]
sockets = ["0.0.0.0:8080"]
insecure_http = false
private_key = "/etc/https-key/https.key"
certificate = "/etc/https-cert/https.crt"

[admin]
insecure_api = true
auth_public_key = "/etc/auth-secret/publicKey"

[attestation_token]
insecure_key = true
attestation_token_type = "CoCo"

[attestation_service]
type = "coco_as_builtin"
work_dir = "/opt/confidential-containers/attestation-service"
policy_engine = "opa"

[attestation_service.attestation_token_broker]
type = "Ear"
policy_dir = "/opt/confidential-containers/attestation-service/policies"

[attestation_service.attestation_token_config]
duration_min = 5

[attestation_service.rvps_config]
type = "BuiltIn"

[attestation_service.rvps_config.storage]
type = "LocalJson"
file_path = "/opt/confidential-containers/rvps/reference-values/reference-values.json"

[[plugins]]
name = "resource"
type = "LocalFs"
dir_path = "/opt/confidential-containers/kbs/repository"

[policy_engine]
policy_path = "/opt/confidential-containers/opa/policy.rego"
"#;

    let mut data = BTreeMap::new();
    data.insert("kbs-config.toml".to_string(), kbs_config_toml.to_string());

    let config_map = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let config_maps: Api<ConfigMap> = Api::namespaced(client, namespace);
    match config_maps
        .create(&PostParams::default(), &config_map)
        .await
    {
        Ok(s) => info!("Created ConfigMap {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => info!("ConfigMap {} already exists", name),
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

pub async fn generate_reference_values(
    client: Client,
    namespace: &str,
    name: &str,
) -> anyhow::Result<()> {
    let reference_values_json = r#"[
    ]"#;

    let mut data = BTreeMap::new();
    data.insert(
        "reference-values.json".to_string(),
        reference_values_json.to_string(),
    );

    let config_map = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let config_maps: Api<ConfigMap> = Api::namespaced(client, namespace);
    match config_maps
        .create(&PostParams::default(), &config_map)
        .await
    {
        Ok(s) => info!("Created ConfigMap {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => info!("ConfigMap {} already exists", name),
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

// TODO: this function needs to be removed, right now it is only for testing the resource
pub async fn generate_secret(client: Client, namespace: &str, name: &str) -> anyhow::Result<()> {
    let mut data = BTreeMap::new();
    data.insert("key".to_string(), k8s_openapi::ByteString(b"test".to_vec()));

    let secret = Secret {
        metadata: kube::api::ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let secrets: Api<Secret> = Api::namespaced(client, namespace);
    match secrets.create(&PostParams::default(), &secret).await {
        Ok(s) => info!("Created Secret {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => info!("Secret {} already exists", name),
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

pub async fn generate_resource_policy(
    client: Client,
    namespace: &str,
    name: &str,
) -> anyhow::Result<()> {
    let policy_rego = r#"package policy
default allow = true
"#;
    let mut data = BTreeMap::new();
    data.insert("policy.rego".to_string(), policy_rego.to_string());

    let config_map = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let config_maps: Api<ConfigMap> = Api::namespaced(client, namespace);
    match config_maps
        .create(&PostParams::default(), &config_map)
        .await
    {
        Ok(s) => info!("Created ConfigMap {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => info!("ConfigMap {} already exists", name),
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

pub async fn generate_kbs(
    client: Client,
    namespace: &str,
    trustee: &Trustee,
    secret: &str,
) -> anyhow::Result<()> {
    let labels = BTreeMap::from([
        (
            "app.kubernetes.io/name".to_string(),
            "kbsconfig".to_string(),
        ),
        (
            "app.kubernetes.io/instance".to_string(),
            "kbsconfig-sample".to_string(),
        ),
        (
            "app.kubernetes.io/part-of".to_string(),
            "kbs-operator".to_string(),
        ),
        (
            "app.kubernetes.io/managed-by".to_string(),
            "kustomize".to_string(),
        ),
        (
            "app.kubernetes.io/created-by".to_string(),
            "kbs-operator".to_string(),
        ),
    ]);

    let kbs_config = KbsConfig {
        metadata: kube::api::ObjectMeta {
            name: Some(trustee.kbs_config_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: KbsConfigSpec {
            kbs_config_map_name: trustee.kbs_configuration.clone(),
            kbs_auth_secret_name: trustee.kbs_auth_key.clone(),
            kbs_deployment_type: "AllInOneDeployment".to_string(),
            kbs_rvps_ref_values_config_map_name: trustee.reference_values.clone(),
            kbs_secret_resources: vec![secret.to_string()],
            kbs_https_key_secret_name: "kbs-https-key".to_string(),
            kbs_https_cert_secret_name: "kbs-https-certificate".to_string(),
            kbs_resource_policy_config_map_name: trustee.resource_policy.clone(),
        },
    };

    let kbs_configs: Api<KbsConfig> = Api::namespaced(client, namespace);
    match kbs_configs
        .create(&PostParams::default(), &kbs_config)
        .await
    {
        Ok(s) => info!("Created KbsConfig {:?}", s.metadata.name),
        Err(Error::Api(ae)) if ae.code == 409 => {
            info!("KbsConfig {} already exists", trustee.kbs_config_name)
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}
