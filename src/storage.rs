use crate::{
    config::{S3, secret},
    digest,
    run::Run,
};
use anyhow::{Context, Result, ensure};
use futures::TryStreamExt;
use object_store::ObjectStoreExt;
use object_store::{
    GetOptions, ObjectStore, PutMultipartOptions, WriteMultipart, aws::AmazonS3Builder, path::Path,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub modified: String,
}

pub fn connect(config: &S3) -> Result<impl ObjectStore + use<>> {
    config.validate()?;
    AmazonS3Builder::new()
        .with_endpoint(&config.endpoint)
        .with_region(&config.region)
        .with_bucket_name(&config.bucket)
        .with_access_key_id(secret(&config.access_key_env)?)
        .with_secret_access_key(secret(&config.secret_key_env)?)
        .with_virtual_hosted_style_request(false)
        .with_allow_http(config.endpoint.starts_with("http://"))
        .build()
        .map_err(|_| anyhow::anyhow!("cannot configure S3 endpoint"))
}

pub async fn inventory(store: &dyn ObjectStore) -> Result<Vec<Object>> {
    let mut objects = store
        .list(None)
        .map_ok(|o| Object {
            key: o.location.to_string(),
            size: o.size,
            etag: o.e_tag,
            modified: o.last_modified.to_rfc3339(),
        })
        .try_collect::<Vec<_>>()
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot list storage; check endpoint, credentials, and list permissions"
            )
        })?;
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    ensure!(
        objects.windows(2).all(|w| w[0].key != w[1].key),
        "duplicate storage keys in enumeration"
    );
    Ok(objects)
}

fn object_path(key: &str) -> Result<Path> {
    let path = Path::parse(key).context("unsupported object key")?;
    ensure!(
        path.as_ref() == key,
        "object key would be normalized; migration blocked"
    );
    Ok(path)
}

pub async fn checksum(
    store: &dyn ObjectStore,
    key: &str,
) -> Result<(u64, String, object_store::Attributes)> {
    let object = store
        .get(&object_path(key)?)
        .await
        .map_err(|_| anyhow::anyhow!("cannot read object"))?;
    let attributes = object.attributes.clone();
    let mut stream = object.into_stream();
    let mut hash = Sha256::new();
    let mut size = 0;
    while let Some(bytes) = stream
        .try_next()
        .await
        .map_err(|_| anyhow::anyhow!("object read interrupted"))?
    {
        size += bytes.len() as u64;
        hash.update(&bytes);
    }
    Ok((size, format!("{:x}", hash.finalize()), attributes))
}

pub async fn transfer(
    source: &dyn ObjectStore,
    target: &dyn ObjectStore,
    objects: &[Object],
    run: &Run,
    bucket_index: usize,
) -> Result<()> {
    ensure!(
        inventory(source).await? == objects,
        "source object inventory changed; create a fresh plan after freezing writes"
    );
    let existing: BTreeMap<_, _> = inventory(target)
        .await?
        .into_iter()
        .map(|o| (o.key.clone(), o))
        .collect();
    ensure!(
        existing.keys().all(|k| objects.iter().any(|o| &o.key == k)),
        "destination contains unexpected objects"
    );
    for o in objects {
        let path = object_path(&o.key)?;
        // Existing objects are never overwritten. Re-read both sides even after a completed journal entry.
        if existing.contains_key(&o.key) {
            let left = checksum(source, &o.key).await?;
            let right = checksum(target, &o.key).await?;
            ensure!(
                left == right,
                "destination object collision; existing bytes or metadata differ"
            );
            run.event("storage", "object_verified", serde_json::json!({"bucket":bucket_index,"key_sha256":digest(o.key.as_bytes()),"sha256":left.1}))?;
            continue;
        }
        let get = source
            .get_opts(
                &path,
                GetOptions {
                    if_match: o.etag.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| anyhow::anyhow!("source object changed or cannot be read"))?;
        ensure!(get.meta.size == o.size, "source object size changed");
        let attributes = get.attributes.clone();
        let upload = target
            .put_multipart_opts(
                &path,
                PutMultipartOptions {
                    attributes: attributes.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| anyhow::anyhow!("cannot start multipart upload"))?;
        let mut writer = WriteMultipart::new(upload);
        let mut stream = get.into_stream();
        let mut hash = Sha256::new();
        let mut size = 0;
        while let Some(bytes) = match stream.try_next().await {
            Ok(value) => value,
            Err(_) => {
                let _ = writer.abort().await;
                anyhow::bail!("source stream interrupted; retry the storage phase");
            }
        } {
            if writer.wait_for_capacity(2).await.is_err() {
                let _ = writer.abort().await;
                anyhow::bail!("multipart upload failed");
            }
            size += bytes.len() as u64;
            hash.update(&bytes);
            writer.put(bytes);
        }
        if size != o.size {
            let _ = writer.abort().await;
            anyhow::bail!("source stream length changed");
        }
        writer.finish().await.map_err(|_| {
            anyhow::anyhow!("upload completion uncertain; retry will verify the destination")
        })?;
        let sha256 = format!("{:x}", hash.finalize());
        let actual = checksum(target, &o.key).await?;
        ensure!(
            actual == (size, sha256.clone(), attributes),
            "destination checksum or metadata mismatch"
        );
        run.event("storage", "object_verified", serde_json::json!({"bucket":bucket_index,"key_sha256":digest(o.key.as_bytes()),"sha256":sha256}))?;
    }
    ensure!(
        inventory(source).await? == objects,
        "source objects changed during transfer"
    );
    Ok(())
}

pub async fn verify(
    source: &dyn ObjectStore,
    target: &dyn ObjectStore,
    objects: &[Object],
) -> Result<()> {
    ensure!(
        inventory(source).await? == objects,
        "source object inventory drift"
    );
    let dest = inventory(target).await?;
    ensure!(
        dest.len() == objects.len()
            && dest
                .iter()
                .zip(objects)
                .all(|(a, b)| a.key == b.key && a.size == b.size),
        "destination object set differs"
    );
    for object in objects {
        ensure!(
            checksum(source, &object.key).await? == checksum(target, &object.key).await?,
            "object content or metadata differs"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    #[tokio::test]
    async fn transfer_verifies_resume_and_rejects_collisions() -> Result<()> {
        let a = InMemory::new();
        let b = InMemory::new();
        a.put(&Path::from("photos/部品.svg"), "<svg/>".into())
            .await?;
        let temp = tempfile::tempdir()?;
        let run = Run::open(&temp.path().join("run"), "test")?;
        let objects = inventory(&a).await?;
        transfer(&a, &b, &objects, &run, 0).await?;
        transfer(&a, &b, &objects, &run, 0).await?;
        verify(&a, &b, &objects).await?;
        b.put(&Path::from("photos/部品.svg"), "different".into())
            .await?;
        assert!(transfer(&a, &b, &objects, &run, 0).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn transfer_preserves_metadata_and_rejects_drift() -> Result<()> {
        use object_store::{Attribute, Attributes, PutOptions};
        let a = InMemory::new();
        let b = InMemory::new();
        let mut attributes = Attributes::new();
        attributes.insert(Attribute::ContentType, "image/svg+xml".into());
        attributes.insert(Attribute::CacheControl, "max-age=3600".into());
        a.put_opts(
            &Path::from("test.svg"),
            "<svg/>".into(),
            PutOptions {
                attributes,
                ..Default::default()
            },
        )
        .await?;
        let temp = tempfile::tempdir()?;
        let run = Run::open(&temp.path().join("run"), "test")?;
        let objects = inventory(&a).await?;
        transfer(&a, &b, &objects, &run, 0).await?;
        verify(&a, &b, &objects).await?;
        a.put(&Path::from("new"), "changed".into()).await?;
        assert!(transfer(&a, &b, &objects, &run, 0).await.is_err());
        assert!(object_path("a//b").is_err());
        Ok(())
    }
}
