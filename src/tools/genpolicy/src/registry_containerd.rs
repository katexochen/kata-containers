// Copyright (c) 2024 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

// Allow Docker image config field names.
#![allow(non_snake_case)]
use crate::layers_cache::ImageLayersCache;
use crate::registry::{
    get_users_from_decompressed_layer, Container, DockerConfigLayer, ImageLayer, WHITEOUT_MARKER,
};
use crate::utils::Config;

use anyhow::{anyhow, bail, Result};
use containerd_client::{services::v1::GetImageRequest, with_namespace};
use docker_credential::{CredentialRetrievalError, DockerCredential};
use k8s_cri::v1::{image_service_client::ImageServiceClient, AuthConfig};
use log::{debug, info, warn};
use oci_client::Reference;
use std::{collections::HashMap, convert::TryFrom, io::Write, path::Path};
use tokio::{
    io,
    io::{AsyncSeekExt, AsyncWriteExt},
    net::UnixStream,
};
use tonic::transport::{Endpoint, Uri};
use tonic::Request;
use tower::service_fn;

impl Container {
    pub async fn new_containerd_pull(
        config: &Config,
        image: &str,
        containerd_socket_path: &str,
    ) -> Result<Self> {
        info!("============================================");
        info!("Using containerd socket: {:?}", containerd_socket_path);

        let ctrd_path = containerd_socket_path.to_string();
        let containerd_channel = Endpoint::try_from("http://[::]")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                UnixStream::connect(ctrd_path.clone())
            }))
            .await?;

        let ctrd_client = containerd_client::Client::from(containerd_channel.clone());
        let k8_cri_image_client = ImageServiceClient::new(containerd_channel);

        let image_str = image.to_string();
        let image_ref: Reference = image_str.parse().unwrap();

        info!("Pulling image: {:?}", image_ref);
        pull_image(&image_ref, k8_cri_image_client.clone()).await?;

        let image_ref_str = &image_ref.to_string();
        let manifest = get_image_manifest(image_ref_str, &ctrd_client).await?;
        debug!(
            "manifest: {}",
            serde_json::to_string_pretty(&manifest).unwrap()
        );

        let config_layer = get_config_layer(image_ref_str, k8_cri_image_client)
            .await
            .unwrap();
        debug!("config_layer: {:?}", &config_layer);

        let mut passwd = String::new();
        let mut group = String::new();

        // Nydus/guest_pull doesn't make available passwd/group files from layers properly.
        // See issue https://github.com/kata-containers/kata-containers/issues/11162
        if config.settings.cluster_config.guest_pull {
            info!("Guest pull is enabled, skipping passwd/group file parsing");
            return Ok(Container {
                image: image_str,
                config_layer,
                passwd,
                group,
            });
        }

        let image_layers =
            get_image_layers(&config.layers_cache, &manifest, &config_layer, &ctrd_client)
                .await
                .unwrap();

        // Find the last layer with an /etc/* file, respecting whiteouts.
        info!("Parsing users and groups in image layers");
        for layer in &image_layers {
            if layer.passwd == WHITEOUT_MARKER {
                passwd = String::new();
            } else if !layer.passwd.is_empty() {
                passwd = layer.passwd.clone();
            }

            if layer.group == WHITEOUT_MARKER {
                group = String::new();
            } else if !layer.group.is_empty() {
                group = layer.group.clone();
            }
        }

        Ok(Container {
            image: image_str,
            config_layer,
            passwd,
            group,
        })
    }
}

pub async fn get_content(
    digest: &str,
    client: &containerd_client::Client,
) -> Result<serde_json::Value, anyhow::Error> {
    let req = containerd_client::services::v1::ReadContentRequest {
        digest: digest.to_string(),
        offset: 0,
        size: 0,
    };
    let req = with_namespace!(req, "k8s.io");
    let mut c = client.content();
    let resp = c.read(req).await?;
    let mut stream = resp.into_inner();

    if let Some(chunk) = stream.message().await? {
        if chunk.offset < 0 {
            return Err(anyhow!("Negative offset in chunk"));
        }
        return Ok(serde_json::from_slice(&chunk.data)?);
    }

    Err(anyhow!("Unable to find content for digest: {}", digest))
}

pub async fn get_image_manifest(
    image_ref: &str,
    client: &containerd_client::Client,
) -> Result<serde_json::Value> {
    let mut imageChannel = client.images();

    let req = GetImageRequest {
        name: image_ref.to_string(),
    };
    let req = with_namespace!(req, "k8s.io");
    let resp = imageChannel.get(req).await?;

    let image_digest = resp.into_inner().image.unwrap().target.unwrap().digest;

    // content may be an image manifest (https://github.com/opencontainers/image-spec/blob/main/manifest.md)
    //or an image index (https://github.com/opencontainers/image-spec/blob/main/image-index.md)
    let content = get_content(&image_digest, client).await?;

    let is_image_manifest = content.get("layers").is_some();

    if is_image_manifest {
        return Ok(content);
    }

    // else, content is an image index
    let image_index = content;

    let manifests = image_index["manifests"].as_array().unwrap();

    let mut manifestAmd64 = &serde_json::Value::Null;

    for entry in manifests {
        let platform = entry["platform"].as_object().unwrap();
        let architecture = platform["architecture"].as_str().unwrap();
        let os = platform["os"].as_str().unwrap();
        if architecture == "amd64" && os == "linux" {
            manifestAmd64 = entry;
            break;
        }
    }

    let image_digest = manifestAmd64["digest"].as_str().unwrap();

    get_content(image_digest, client).await
}

pub async fn get_config_layer(
    image_ref: &str,
    mut client: ImageServiceClient<tonic::transport::Channel>,
) -> Result<DockerConfigLayer> {
    let req = k8s_cri::v1::ImageStatusRequest {
        image: Some(k8s_cri::v1::ImageSpec {
            image: image_ref.to_string(),
            annotations: HashMap::new(),
        }),
        verbose: true,
    };

    let resp = client.image_status(req).await?;
    let image_layers = resp.into_inner();

    let status_info: serde_json::Value =
        serde_json::from_str(image_layers.info.get("info").unwrap())?;
    let image_spec = status_info["imageSpec"].as_object().unwrap();
    let docker_config_layer: DockerConfigLayer =
        serde_json::from_value(serde_json::to_value(image_spec)?)?;

    Ok(docker_config_layer)
}

pub async fn pull_image(
    image_ref: &Reference,
    mut client: ImageServiceClient<tonic::transport::Channel>,
) -> Result<()> {
    let auth = build_auth(image_ref);

    debug!("cri auth: {:?}", auth);

    let req = k8s_cri::v1::PullImageRequest {
        image: Some(k8s_cri::v1::ImageSpec {
            image: image_ref.to_string(),
            annotations: HashMap::new(),
        }),
        auth,
        sandbox_config: None,
    };

    client.pull_image(req).await?;

    Ok(())
}

pub fn build_auth(reference: &Reference) -> Option<AuthConfig> {
    debug!("build_auth: {:?}", reference);

    let server = reference
        .resolve_registry()
        .strip_suffix('/')
        .unwrap_or_else(|| reference.resolve_registry());

    debug!("server: {:?}", server);

    match docker_credential::get_credential(server) {
        Ok(DockerCredential::UsernamePassword(username, password)) => {
            debug!("build_auth: Found docker credentials");
            return Some(AuthConfig {
                username,
                password,
                auth: "".to_string(),
                server_address: "".to_string(),
                identity_token: "".to_string(),
                registry_token: "".to_string(),
            });
        }
        Ok(DockerCredential::IdentityToken(identity_token)) => {
            debug!("build_auth: Found identity token");
            return Some(AuthConfig {
                username: "".to_string(),
                password: "".to_string(),
                auth: "".to_string(),
                server_address: "".to_string(),
                identity_token,
                registry_token: "".to_string(),
            });
        }
        Err(CredentialRetrievalError::ConfigNotFound) => {
            debug!("build_auth: Docker config not found - using anonymous access.");
        }
        Err(CredentialRetrievalError::NoCredentialConfigured) => {
            debug!("build_auth: Docker credentials not configured - using anonymous access.");
        }
        Err(CredentialRetrievalError::ConfigReadError) => {
            debug!("build_auth: Cannot read docker credentials - using anonymous access.");
        }
        Err(CredentialRetrievalError::HelperFailure { stdout, stderr }) => {
            if stdout == "credentials not found in native keychain\n" {
                // On WSL, this error is generated when credentials are not
                // available in ~/.docker/config.json.
                debug!("build_auth: Docker credentials not found - using anonymous access.");
            } else {
                warn!("build_auth: Docker credentials not found - using anonymous access. stderr = {}, stdout = {}",
                    &stderr, &stdout);
            }
        }
        Err(e) => panic!("Error handling docker configuration file: {}", e),
    }

    None
}

pub async fn get_image_layers(
    layers_cache: &ImageLayersCache,
    manifest: &serde_json::Value,
    config_layer: &DockerConfigLayer,
    client: &containerd_client::Client,
) -> Result<Vec<ImageLayer>> {
    let mut layer_index = 0;
    let mut layersVec = Vec::new();

    let layers = manifest["layers"].as_array().unwrap();

    for layer in layers {
        let layer_media_type = layer["mediaType"].as_str().unwrap();
        if layer_media_type.eq("application/vnd.docker.image.rootfs.diff.tar.gzip")
            || layer_media_type.eq("application/vnd.oci.image.layer.v1.tar+gzip")
        {
            if layer_index < config_layer.rootfs.diff_ids.len() {
                let mut imageLayer = get_users_from_layer(
                    layers_cache,
                    layer["digest"].as_str().unwrap(),
                    client,
                    &config_layer.rootfs.diff_ids[layer_index].clone(),
                )
                .await?;
                imageLayer.diff_id = config_layer.rootfs.diff_ids[layer_index].clone();
                layersVec.push(imageLayer);
            } else {
                return Err(anyhow!("Too many Docker gzip layers"));
            }
            layer_index += 1;
        }
    }

    Ok(layersVec)
}

async fn get_users_from_layer(
    layers_cache: &ImageLayersCache,
    layer_digest: &str,
    client: &containerd_client::Client,
    diff_id: &str,
) -> Result<ImageLayer> {
    if let Some(layer) = layers_cache.get_layer(diff_id) {
        info!("Using cache file");
        return Ok(layer);
    }

    let temp_dir = tempfile::tempdir_in(".")?;
    let base_dir = temp_dir.path();
    // Use file names supported by both Linux and Windows.
    let file_name = str::replace(layer_digest, ":", "-");
    let mut decompressed_path = base_dir.join(file_name);
    decompressed_path.set_extension("tar");

    let mut compressed_path = decompressed_path.clone();
    compressed_path.set_extension("gz");

    // go find verity hash if not found in cache
    if let Err(e) =
        create_decompressed_layer_file(client, layer_digest, &decompressed_path, &compressed_path)
            .await
    {
        temp_dir.close()?;
        bail!(format!(
            "Failed to create verity hash for {layer_digest}, error {e}"
        ));
    }

    match get_users_from_decompressed_layer(&decompressed_path) {
        Err(e) => {
            temp_dir.close()?;
            bail!(format!("Failed to get verity hash {e}"));
        }
        Ok((passwd, group)) => {
            let layer = ImageLayer {
                diff_id: diff_id.to_string(),
                passwd,
                group,
            };
            layers_cache.insert_layer(&layer);
            Ok(layer)
        }
    }
}

async fn create_decompressed_layer_file(
    client: &containerd_client::Client,
    layer_digest: &str,
    decompressed_path: &Path,
    compressed_path: &Path,
) -> Result<()> {
    info!("Pulling layer {layer_digest}");
    let mut file = tokio::fs::File::create(&compressed_path)
        .await
        .map_err(|e| anyhow!(e))?;

    info!("Decompressing layer");

    let req = containerd_client::services::v1::ReadContentRequest {
        digest: layer_digest.to_string(),
        offset: 0,
        size: 0,
    };
    let req = with_namespace!(req, "k8s.io");
    let mut c = client.content();
    let resp = c.read(req).await?;
    let mut stream = resp.into_inner();

    while let Some(chunk) = stream.message().await? {
        if chunk.offset < 0 {
            return Err(anyhow!("Too many Docker gzip layers"));
        }
        file.seek(io::SeekFrom::Start(chunk.offset as u64)).await?;
        file.write_all(&chunk.data).await?;
    }

    file.flush()
        .await
        .map_err(|e| anyhow!(e))
        .expect("Failed to flush file");
    let compressed_file = std::fs::File::open(compressed_path).map_err(|e| anyhow!(e))?;
    let mut decompressed_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(decompressed_path)?;
    let mut gz_decoder = flate2::read::GzDecoder::new(compressed_file);
    std::io::copy(&mut gz_decoder, &mut decompressed_file).map_err(|e| anyhow!(e))?;

    decompressed_file.flush().map_err(|e| anyhow!(e))?;
    Ok(())
}
