use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub jwt_secret: String,
    pub service_key: String,
    pub anon_key: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub s3_endpoint: Option<String>,
    pub s3_force_path_style: bool,
    pub imgproxy_url: Option<String>,
    pub file_size_limit: u64,
    pub storage_quota_bytes: Option<u64>,
    pub signed_url_expiry_secs: u64,
    pub tenant_id: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Config {
            port: env::var("PORT").unwrap_or_else(|_| "5000".into()).parse()?,
            database_url: env::var("DATABASE_URL")?,
            jwt_secret: env::var("PGRST_JWT_SECRET")
                .or_else(|_| env::var("JWT_SECRET"))?,
            service_key: env::var("SERVICE_KEY").unwrap_or_default(),
            anon_key: env::var("ANON_KEY").unwrap_or_default(),
            s3_bucket: env::var("GLOBAL_S3_BUCKET")?,
            s3_region: env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
            s3_endpoint: env::var("GLOBAL_S3_ENDPOINT").ok().filter(|s| !s.is_empty()),
            s3_force_path_style: env::var("GLOBAL_S3_FORCE_PATH_STYLE")
                .unwrap_or_default()
                .eq_ignore_ascii_case("true"),
            imgproxy_url: env::var("IMGPROXY_URL").ok().filter(|s| !s.is_empty()),
            file_size_limit: env::var("FILE_SIZE_LIMIT")
                .unwrap_or_else(|_| "52428800".into())
                .parse()?,
            storage_quota_bytes: env::var("STORAGE_QUOTA_BYTES").ok()
                .map(|v| v.parse::<u64>())
                .transpose()
                .map_err(|_| anyhow::anyhow!("STORAGE_QUOTA_BYTES must be a positive integer"))?,
            signed_url_expiry_secs: env::var("UPLOAD_SIGNED_URL_EXPIRATION_TIME")
                .unwrap_or_else(|_| "120".into())
                .parse()?,
            tenant_id: env::var("TENANT_ID").unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn port_parse() {
        let p: u16 = "5000".parse().unwrap();
        assert_eq!(p, 5000);
    }

    #[test]
    fn file_size_limit_parse() {
        let v: u64 = "52428800".parse().unwrap();
        assert_eq!(v, 52_428_800);
    }
}
