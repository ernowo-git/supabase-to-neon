use crate::{database::Connection, write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub version: u32,
    pub plan_digest: String,
    pub mappings: Vec<Mapping>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub source_id: Uuid,
    pub target_subject: String,
    pub approved_by: String,
    pub proof_reference: String,
}

pub fn export(users: &Value, plan_digest: &str, path: &Path) -> Result<()> {
    write_json(
        path,
        &serde_json::json!({
            "version":1,"plan_digest":plan_digest,"users":users["users"],
            "instructions":"Enroll accounts through supported Neon Auth flows. Preserve source UUIDs as application principals. Submit verified source-to-target mappings with approved_by and proof_reference. Email equality alone is insufficient. Sessions, password hashes, and MFA secrets are not transferred."
        }),
    )
}

pub fn validate(
    enrollment: &Enrollment,
    auth: &Value,
    plan_digest: &str,
    target_users: &Value,
) -> Result<()> {
    ensure!(
        enrollment.version == 1 && enrollment.plan_digest == plan_digest,
        "enrollment belongs to another plan or version"
    );
    let source = auth["users"].as_array().context("invalid source users")?;
    let target = target_users.as_array().context("invalid target users")?;
    let mut ids = BTreeSet::new();
    let mut subjects = BTreeSet::new();
    for mapping in &enrollment.mappings {
        ensure!(
            ids.insert(mapping.source_id) && subjects.insert(&mapping.target_subject),
            "duplicate identity mapping"
        );
        ensure!(
            !mapping.approved_by.trim().is_empty() && !mapping.proof_reference.trim().is_empty(),
            "identity mapping requires an approver and proof reference"
        );
        ensure!(
            source
                .iter()
                .any(|s| s["id"].as_str() == Some(mapping.source_id.to_string().as_str())),
            "mapping references an unknown source user"
        );
        ensure!(
            target.iter().any(
                |t| t["id"].as_str() == Some(mapping.target_subject.as_str())
                    && t["disabled"] == false
            ),
            "mapped account is missing or disabled in target Auth"
        );
    }
    for user in source {
        let disabled = user["disabled"].as_bool().unwrap_or(false);
        let id: Uuid = user["id"]
            .as_str()
            .context("missing source identity")?
            .parse()?;
        ensure!(
            disabled || ids.contains(&id),
            "active user has not completed verified enrollment"
        );
        ensure!(
            !disabled || !ids.contains(&id),
            "disabled source users must remain unmapped"
        );
    }
    Ok(())
}

pub async fn target_users(conn: &Connection) -> Result<Value> {
    let exists = conn
        .query("SELECT to_json(to_regclass('neon_auth.user') IS NOT NULL);")
        .await?;
    ensure!(
        exists == true,
        "target Neon Auth user table unavailable; complete provider setup first"
    );
    conn.query("SELECT COALESCE(json_agg(json_build_object('id',to_jsonb(u)->>'id','disabled',COALESCE((to_jsonb(u)->>'banned')::boolean,false) OR COALESCE((to_jsonb(u)->>'disabled')::boolean,false)) ORDER BY to_jsonb(u)->>'id'),'[]'::json) FROM neon_auth.\"user\" u;").await
}

pub fn sql(enrollment: &Enrollment, auth: &Value) -> Result<String> {
    let mut sql = String::from(
        "CREATE SCHEMA migration_identity;\nREVOKE ALL ON SCHEMA migration_identity FROM PUBLIC;\nCREATE TABLE migration_identity.principal (id uuid PRIMARY KEY, disabled boolean NOT NULL, created_at timestamptz NOT NULL DEFAULT now());\nCREATE TABLE migration_identity.provider_identity (principal_id uuid PRIMARY KEY REFERENCES migration_identity.principal(id), provider text NOT NULL, subject text NOT NULL, UNIQUE(provider,subject));\nALTER TABLE migration_identity.principal ENABLE ROW LEVEL SECURITY;\nALTER TABLE migration_identity.provider_identity ENABLE ROW LEVEL SECURITY;\nREVOKE ALL ON ALL TABLES IN SCHEMA migration_identity FROM PUBLIC;\n",
    );
    for u in auth["users"].as_array().context("invalid auth inventory")? {
        let id: Uuid = u["id"].as_str().context("missing identity")?.parse()?;
        let disabled = u["disabled"].as_bool().unwrap_or(false);
        sql.push_str(&format!(
            "INSERT INTO migration_identity.principal(id,disabled) VALUES ('{id}',{disabled});\n"
        ));
    }
    for m in &enrollment.mappings {
        ensure!(!m.target_subject.contains('\0'), "invalid target subject");
        let subject = m.target_subject.replace(char::from(39), "''");
        sql.push_str(&format!("INSERT INTO migration_identity.provider_identity(principal_id,provider,subject) VALUES ('{}','neon',E'{}');\n",m.source_id,subject.replace('\\', "\\\\")));
    }
    Ok(sql)
}

pub async fn verify(
    conn: &Connection,
    enrollment: &Enrollment,
    auth: &Value,
    plan_digest: &str,
) -> Result<()> {
    validate(enrollment, auth, plan_digest, &target_users(conn).await?)?;
    let actual = conn.query("SELECT json_build_object('principals',COALESCE((SELECT json_agg(json_build_object('id',id,'disabled',disabled) ORDER BY id) FROM migration_identity.principal),'[]'::json),'mappings',COALESCE((SELECT json_agg(json_build_object('source_id',principal_id,'target_subject',subject) ORDER BY principal_id) FROM migration_identity.provider_identity WHERE provider='neon'),'[]'::json));").await?;
    let source = auth["users"].as_array().context("invalid users")?;
    let principals = actual["principals"]
        .as_array()
        .context("missing target principals")?;
    ensure!(principals.len() == source.len(), "principal count differs");
    for u in source {
        ensure!(
            principals.iter().any(|p| p["id"] == u["id"]
                && p["disabled"].as_bool().unwrap_or(false)
                    == u["disabled"].as_bool().unwrap_or(false)),
            "principal differs"
        );
    }
    let mappings = actual["mappings"].as_array().context("missing mappings")?;
    ensure!(
        mappings.len() == enrollment.mappings.len(),
        "identity mapping count differs"
    );
    for m in &enrollment.mappings {
        ensure!(
            mappings
                .iter()
                .any(|a| a["source_id"] == m.source_id.to_string()
                    && a["target_subject"] == m.target_subject),
            "identity mapping differs"
        );
    }
    Ok(())
}
