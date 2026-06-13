use aws_sdk_s3::{
    config::{Builder as S3ConfigBuilder, Region},
    operation::get_object::GetObjectOutput,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier},
    Client,
};
use bytes::Bytes;

use crate::{
    config::Config,
    error::{Result, StorageError},
};

pub struct S3Client {
    inner: Client,
    bucket: String,
    tenant_id: String,
}

impl S3Client {
    pub async fn new(config: &Config) -> Self {
        let sdk_config = aws_config::load_from_env().await;

        let mut builder = S3ConfigBuilder::from(&sdk_config)
            .region(Region::new(config.s3_region.clone()))
            .force_path_style(config.s3_force_path_style);

        if let Some(ep) = &config.s3_endpoint {
            builder = builder.endpoint_url(ep);
        }

        let inner = Client::from_conf(builder.build());
        S3Client {
            inner,
            bucket: config.s3_bucket.clone(),
            tenant_id: config.tenant_id.clone(),
        }
    }

    // S3 key: {tenant_id}/{bucket_id}/{object_name}
    fn key(&self, bucket_id: &str, object_name: &str) -> String {
        if self.tenant_id.is_empty() {
            format!("{}/{}", bucket_id, object_name)
        } else {
            format!("{}/{}/{}", self.tenant_id, bucket_id, object_name)
        }
    }

    // Exposed for imgproxy URL construction.
    pub fn object_key(&self, bucket_id: &str, object_name: &str) -> String {
        self.key(bucket_id, object_name)
    }

    pub fn s3_bucket(&self) -> &str {
        &self.bucket
    }

    pub async fn put(&self, bucket_id: &str, name: &str, content_type: &str, data: Bytes) -> Result<()> {
        self.inner
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .content_type(content_type)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;
        Ok(())
    }

    pub async fn get(&self, bucket_id: &str, name: &str) -> Result<GetObjectOutput> {
        self.inner
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .send()
            .await
            .map_err(|_| StorageError::NotFound)
    }

    /// Returns `(etag, last_modified_http_date)` from S3 for use in HEAD responses.
    pub async fn head_meta(
        &self,
        bucket_id: &str,
        name: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let out = self.inner
            .head_object()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .send()
            .await
            .map_err(|_| StorageError::NotFound)?;

        let etag = out.e_tag.clone();
        let last_modified = out.last_modified.and_then(|dt| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(dt.secs(), dt.subsec_nanos())
                .map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        });
        Ok((etag, last_modified))
    }

    pub async fn delete(&self, bucket_id: &str, name: &str) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;
        Ok(())
    }

    // Batch delete up to 1000 objects per S3 API call.
    pub async fn delete_many(&self, objects: &[(String, String)]) -> Result<()> {
        for chunk in objects.chunks(1000) {
            let identifiers: Vec<ObjectIdentifier> = chunk
                .iter()
                .filter_map(|(b, n)| {
                    ObjectIdentifier::builder()
                        .key(self.key(b, n))
                        .build()
                        .ok()
                })
                .collect();

            if identifiers.is_empty() {
                continue;
            }

            let delete = Delete::builder()
                .set_objects(Some(identifiers))
                .build()
                .map_err(|e| StorageError::S3(e.to_string()))?;

            self.inner
                .delete_objects()
                .bucket(&self.bucket)
                .delete(delete)
                .send()
                .await
                .map_err(|e| StorageError::S3(e.to_string()))?;
        }
        Ok(())
    }

    // Server-side copy within the same S3 bucket.
    pub async fn copy(
        &self,
        src_bucket_id: &str,
        src_name: &str,
        dst_bucket_id: &str,
        dst_name: &str,
    ) -> Result<()> {
        let src_key = self.key(src_bucket_id, src_name);
        let dst_key = self.key(dst_bucket_id, dst_name);
        // copy_source format expected by S3: "bucket/key"
        let copy_source = format!("{}/{}", self.bucket, src_key);

        self.inner
            .copy_object()
            .copy_source(copy_source)
            .bucket(&self.bucket)
            .key(dst_key)
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;
        Ok(())
    }

    // ── TUS / S3 Multipart ────────────────────────────────────────────────────

    pub async fn create_multipart(
        &self,
        bucket_id: &str,
        name: &str,
        content_type: &str,
    ) -> Result<String> {
        let resp = self.inner
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;

        resp.upload_id
            .ok_or_else(|| StorageError::S3("no upload_id".into()))
    }

    pub async fn upload_part(
        &self,
        bucket_id: &str,
        name: &str,
        s3_upload_id: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<String> {
        let resp = self.inner
            .upload_part()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .upload_id(s3_upload_id)
            .part_number(part_number)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;

        resp.e_tag
            .ok_or_else(|| StorageError::S3("no etag".into()))
    }

    pub async fn complete_multipart(
        &self,
        bucket_id: &str,
        name: &str,
        s3_upload_id: &str,
        parts: Vec<(i32, String)>,
    ) -> Result<()> {
        let completed = parts
            .into_iter()
            .map(|(num, etag)| {
                CompletedPart::builder()
                    .part_number(num)
                    .e_tag(etag)
                    .build()
            })
            .collect::<Vec<_>>();

        self.inner
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .upload_id(s3_upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;
        Ok(())
    }

    pub async fn abort_multipart(
        &self,
        bucket_id: &str,
        name: &str,
        s3_upload_id: &str,
    ) -> Result<()> {
        self.inner
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(self.key(bucket_id, name))
            .upload_id(s3_upload_id)
            .send()
            .await
            .map_err(|e| StorageError::S3(e.to_string()))?;
        Ok(())
    }

    pub async fn ensure_bucket_exists(&self) -> anyhow::Result<()> {
        let result = self.inner
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("BucketAlreadyExists")
                    || msg.contains("BucketAlreadyOwnedByYou")
                    || msg.contains("409")
                {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("S3 bucket create failed: {}", e))
                }
            }
        }
    }
}
