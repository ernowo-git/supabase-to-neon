use crate::{
    config::Config,
    database::{self, Connection},
    digest, storage,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub version: u32,
    pub config: Config,
    pub source_identity: String,
    pub target_identity: String,
    pub tools: BTreeMap<String, String>,
    pub database: Value,
    pub target_database: Value,
    pub auth: Value,
    pub buckets: Vec<Vec<storage::Object>>,
    pub source_buckets: Value,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub inventory: Inventory,
    pub blockers: Vec<String>,
    pub manual_actions: Vec<String>,
    pub digest: String,
}

pub async fn inspect(config: Config) -> Result<Inventory> {
    let source = Connection::load(&config.source)?;
    let target = Connection::load(&config.target)?;
    database::assert_distinct(&source, &target)?;
    let tools = database::tools().await?;
    let database = database::inventory(&source, &config.schemas).await?;
    let target_database = database::inventory(&target, &config.schemas).await?;
    let auth = database::auth_inventory(&source).await?;
    let has_storage = source
        .query("SELECT to_json(to_regclass('storage.buckets') IS NOT NULL);")
        .await?;
    let source_buckets = if has_storage == true {
        source.query("SELECT COALESCE(json_agg(json_build_object('name',b.name,'public',b.public,'object_count',(SELECT count(*) FROM storage.objects o WHERE o.bucket_id=b.id)) ORDER BY b.name),'[]'::json) FROM storage.buckets b;").await?
    } else {
        json!([])
    };
    let mut buckets = vec![];
    for b in &config.buckets {
        buckets.push(storage::inventory(&storage::connect(&b.source)?).await?);
    }
    Ok(Inventory {
        version: 1,
        config,
        source_identity: source.identity,
        target_identity: target.identity,
        tools,
        database,
        target_database,
        auth,
        buckets,
        source_buckets,
    })
}

pub fn build(inventory: Inventory) -> Result<Plan> {
    ensure!(inventory.version == 1, "unsupported inventory version");
    inventory.config.validate()?;
    ensure!(
        inventory.source_identity != inventory.target_identity,
        "source and target must differ"
    );
    let mut blockers = BTreeSet::new();
    let db = &inventory.database;
    for key in [
        "tables",
        "functions",
        "views",
        "policies",
        "extensions",
        "foreign_keys",
        "sequences",
        "triggers",
        "grants",
        "publications",
        "foreign_servers",
        "types",
        "rules",
        "schema_names",
    ] {
        ensure!(db[key].is_array(), "database inventory is incomplete");
    }
    ensure!(
        inventory.config.schemas.iter().all(|s| db["schema_names"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == s))),
        "selected source schema does not exist"
    );
    ensure!(
        inventory.auth["users"].is_array()
            && inventory.auth["providers"].is_array()
            && inventory.auth["mfa_factors"].is_number(),
        "Auth inventory is incomplete"
    );
    ensure!(
        inventory.buckets.len() == inventory.config.buckets.len(),
        "storage inventory is incomplete"
    );
    ensure!(
        inventory.target_database["tables"].is_array(),
        "target database inventory is incomplete"
    );
    for key in ["tables", "views", "functions", "sequences", "types"] {
        if inventory.target_database[key]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            blockers.insert("Target application schemas must be empty before planning.".into());
        }
    }
    let source_major = db["server_version"]
        .as_str()
        .context("missing source Postgres version")?
        .parse::<u32>()?
        / 10000;
    let target_major = inventory.target_database["server_version"]
        .as_str()
        .context("missing target Postgres version")?
        .parse::<u32>()?
        / 10000;
    if source_major != target_major {
        blockers.insert(
            "Source and target PostgreSQL major versions must match for this alpha.".into(),
        );
    }
    for tool in ["psql", "pg_dump", "pg_restore"] {
        let version = inventory
            .tools
            .get(tool)
            .context("tool version is missing")?;
        let major = version
            .split_whitespace()
            .find_map(|v| v.split('.').next()?.parse::<u32>().ok());
        if major != Some(source_major) {
            blockers.insert("PostgreSQL tools must match the source major version.".into());
        }
    }
    for fk in db["foreign_keys"]
        .as_array()
        .context("missing foreign keys")?
    {
        if !inventory
            .config
            .schemas
            .iter()
            .any(|s| Some(s.as_str()) == fk["target_schema"].as_str())
        {
            blockers.insert("Cross-schema foreign keys require a reviewed application migration before this alpha can restore them.".into());
        }
    }
    for key in [
        "functions",
        "views",
        "policies",
        "triggers",
        "rules",
        "types",
    ] {
        let text = serde_json::to_string(&db[key])?
            .to_lowercase()
            .replace(['"', '\\'], "");
        if [
            "auth.",
            "storage.",
            "vault.",
            "realtime.",
            "net.",
            "supabase_",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            blockers.insert("Managed-service SQL references require reviewed replacements before database transfer.".into());
        }
    }
    if db["functions"]
        .as_array()
        .is_some_and(|v| v.iter().any(|f| f["security_definer"] == true))
    {
        blockers.insert("SECURITY DEFINER functions require reviewed target ownership and execution grants before transfer.".into());
    }
    if inventory.auth["users"]
        .as_array()
        .is_some_and(|v| v.iter().any(|u| u["phone_user"] == true))
    {
        blockers.insert(
            "Phone accounts require a separately proven target authentication flow.".into(),
        );
    }
    if inventory.auth["providers"].as_array().is_some_and(|v| {
        v.iter().any(|p| {
            p.as_str()
                .is_some_and(|s| s.starts_with("sso:") || matches!(s, "saml" | "web3" | "phone"))
        })
    }) {
        blockers.insert(
            "Unsupported SAML, phone, or Web3 identities require an approved replacement flow."
                .into(),
        );
    }
    if db["policies"].as_array().is_some_and(|v| !v.is_empty()) {
        blockers.insert("Existing policies need reviewed target-role and identity translations; this alpha does not rewrite SQL.".into());
    }
    if db["foreign_servers"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
        || db["tables"]
            .as_array()
            .is_some_and(|v| v.iter().any(|t| t["relkind"] == "f"))
    {
        blockers.insert("Foreign data wrappers require an explicit target integration.".into());
    }
    for table in db["tables"].as_array().context("missing tables")? {
        if table["relkind"] == "p" {
            blockers.insert(
                "Partitioned tables need a separate restore and verification proof.".into(),
            );
        }
    }
    for ext in db["extensions"].as_array().context("missing extensions")? {
        if ext["name"] != "plpgsql" {
            blockers.insert("Non-default extensions require a tested target preparation migration; not automated in this alpha.".into());
        }
    }
    let configured: BTreeSet<_> = inventory
        .config
        .buckets
        .iter()
        .map(|b| b.source.bucket.as_str())
        .collect();
    for b in inventory
        .source_buckets
        .as_array()
        .context("invalid bucket inventory")?
    {
        let name = b["name"].as_str().context("invalid bucket name")?;
        if !configured.contains(name) {
            blockers
                .insert("Every source storage bucket must have an explicit target mapping.".into());
        }
        if let Some(mapping) = inventory
            .config
            .buckets
            .iter()
            .find(|x| x.source.bucket == name)
        {
            let public = matches!(mapping.access, crate::config::Access::Public);
            if b["public"].as_bool() != Some(public) {
                blockers.insert("Bucket mapping changes public/private intent.".into());
            }
            let index = inventory
                .config
                .buckets
                .iter()
                .position(|x| x.source.bucket == mapping.source.bucket)
                .context("missing bucket mapping")?;
            if b["object_count"].as_u64() != Some(inventory.buckets[index].len() as u64) {
                blockers.insert(
                    "Storage database metadata and S3 object enumeration counts differ.".into(),
                );
            }
        }
    }
    let manual_actions = vec![
        "Review SQL dependencies, dynamic SQL, cron jobs, Edge Functions, webhooks, and application service calls. Static inspection cannot prove their absence.".into(),
        "Enroll users through supported Neon Auth flows and approve identity mappings. Passwords, sessions, and MFA secrets are not imported.".into(),
        "Configure destination bucket visibility and test private denials and public access. S3 object transfer does not translate Storage RLS.".into(),
        "Adapt application queries, principal foreign keys, RLS, URL generation, uploads, and Realtime. No application code is rewritten.".into(),
        "Freeze database, Auth, storage, and background writes before the final snapshot. Reconcile recovery after target writes open.".into(),
    ];
    let mut plan = Plan {
        version: 1,
        inventory,
        blockers: blockers.into_iter().collect(),
        manual_actions,
        digest: String::new(),
    };
    plan.digest = plan.computed_digest()?;
    Ok(plan)
}

impl Plan {
    fn computed_digest(&self) -> Result<String> {
        let mut copy = self.clone();
        copy.digest.clear();
        Ok(digest(&serde_json::to_vec(&copy)?))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.digest == self.computed_digest()?,
            "plan is corrupted or has changed"
        );
        let rebuilt = build(self.inventory.clone())?;
        ensure!(
            self.digest == rebuilt.digest,
            "plan policy differs; generate a fresh plan"
        );
        Ok(())
    }
    pub async fn connections(&self) -> Result<(Connection, Connection)> {
        self.validate()?;
        let source = Connection::load(&self.inventory.config.source)?;
        let target = Connection::load(&self.inventory.config.target)?;
        database::assert_distinct(&source, &target)?;
        ensure!(
            source.identity == self.inventory.source_identity
                && target.identity == self.inventory.target_identity,
            "database endpoint drift"
        );
        ensure!(
            database::tools().await? == self.inventory.tools,
            "PostgreSQL tool versions changed"
        );
        ensure!(
            database::inventory(&source, &self.inventory.config.schemas).await?
                == self.inventory.database,
            "source schema drift; inspect and plan again"
        );
        ensure!(
            database::auth_inventory(&source).await? == self.inventory.auth,
            "source Auth inventory drift; inspect and plan again"
        );
        Ok((source, target))
    }
}

pub fn validate_evidence(evidence: &Value, plan_digest: &str, now: u64) -> Result<()> {
    ensure!(
        evidence["plan_digest"] == plan_digest,
        "cutover evidence belongs to a different plan"
    );
    let recorded = evidence["recorded_at_unix"]
        .as_u64()
        .context("cutover evidence requires recorded_at_unix")?;
    ensure!(
        recorded <= now && now - recorded <= 3600,
        "cutover evidence is stale or dated in the future"
    );
    for key in [
        "sql_and_runtime_rls_review",
        "verified_auth_flows",
        "storage_access_tests",
        "application_smoke_tests",
        "background_integrations",
        "writes_frozen",
        "recovery_rehearsal",
    ] {
        ensure!(
            evidence[key]
                .as_str()
                .is_some_and(|v| !v.trim().is_empty() && !v.contains("REPLACE_")),
            "cutover evidence is incomplete; see examples/evidence.json"
        );
    }
    Ok(())
}
