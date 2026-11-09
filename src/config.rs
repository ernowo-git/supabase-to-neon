use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};
use url::Url;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub source: Database,
    pub target: Database,
    pub schemas: Vec<String>,
    #[serde(default)]
    pub buckets: Vec<Bucket>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub project: String,
    pub connection_env: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bucket {
    pub source: S3,
    pub target: S3,
    pub access: Access,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Public,
    Private,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct S3 {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_env: String,
    pub secret_key_env: String,
}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

pub fn secret(name: &str) -> Result<String> {
    ensure!(
        identifier(name),
        "invalid credential environment variable name"
    );
    let value =
        std::env::var(name).with_context(|| format!("missing environment variable {name}"))?;
    ensure!(
        !value.is_empty(),
        "empty credential environment variable {name}"
    );
    Ok(value)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self =
            toml::from_str(&std::fs::read_to_string(path).context("cannot read config")?)
                .context("invalid config TOML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported config version");
        ensure!(
            self.source.project != self.target.project,
            "source and target projects must differ"
        );
        ensure!(
            !self.source.project.is_empty() && !self.target.project.is_empty(),
            "project identities are required"
        );
        ensure!(
            identifier(&self.source.connection_env) && identifier(&self.target.connection_env),
            "invalid database environment variable name"
        );
        ensure!(
            self.source.connection_env != self.target.connection_env,
            "source and target credential variables must differ"
        );
        ensure!(
            !self.schemas.is_empty(),
            "at least one application schema is required"
        );
        let mut schemas = BTreeSet::new();
        for s in &self.schemas {
            ensure!(identifier(s), "schema names must be simple SQL identifiers");
            ensure!(
                !matches!(
                    s.as_str(),
                    "auth"
                        | "storage"
                        | "realtime"
                        | "extensions"
                        | "vault"
                        | "net"
                        | "cron"
                        | "supabase_functions"
                        | "supabase_migrations"
                        | "graphql"
                        | "graphql_public"
                        | "information_schema"
                ) && !s.starts_with("pg_"),
                "managed schemas cannot be exported as application schemas"
            );
            ensure!(schemas.insert(s), "duplicate application schema");
        }
        let mut destinations = BTreeSet::new();
        for b in &self.buckets {
            b.source.validate()?;
            b.target.validate()?;
            ensure!(
                (b.source.endpoint.clone(), b.source.bucket.clone())
                    != (b.target.endpoint.clone(), b.target.bucket.clone()),
                "source and target storage must differ"
            );
            ensure!(
                destinations.insert((&b.target.endpoint, &b.target.bucket)),
                "multiple sources cannot share a destination bucket"
            );
        }
        Ok(())
    }
}

impl S3 {
    pub fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.endpoint).context("invalid storage endpoint")?;
        let local = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        ensure!(
            url.host_str().is_some()
                && (url.scheme() == "https" || (local && url.scheme() == "http")),
            "remote storage endpoints require HTTPS"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "storage endpoints cannot contain credentials, queries, or fragments"
        );
        ensure!(
            !self.bucket.is_empty() && !self.bucket.contains('/') && !self.region.is_empty(),
            "invalid bucket or region"
        );
        ensure!(
            identifier(&self.access_key_env) && identifier(&self.secret_key_env),
            "invalid storage credential variable name"
        );
        Ok(())
    }
}
