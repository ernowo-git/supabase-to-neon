use crate::{database::Connection, digest, plan::Inventory};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const SCHEMA_KEYS: &[&str] = &[
    "schema_names",
    "tables",
    "views",
    "functions",
    "policies",
    "triggers",
    "sequences",
    "foreign_keys",
    "types",
    "rules",
    "extensions",
    "grants",
    "routine_grants",
];

/// Trusted operator-authored SQL, bound to both source and expected target inventories.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adaptation {
    pub version: u32,
    pub source_schema_sha256: String,
    pub reviewed_by: String,
    pub review_reference: String,
    pub before_restore_sql: String,
    pub after_restore_sql: String,
    pub exclude_toc: Vec<String>,
    pub expected_database: Value,
    pub authorization_checks: Vec<AuthorizationCheck>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationCheck {
    pub name: String,
    pub role: String,
    pub actor_id: Option<uuid::Uuid>,
    pub sql: String,
    pub expected: Value,
}

pub fn source_digest(inventory: &Inventory) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&inventory.database)?))
}

pub fn template(inventory: &Inventory, expected: Option<&Inventory>) -> Result<Adaptation> {
    Ok(Adaptation {
        version: 1,
        source_schema_sha256: source_digest(inventory)?,
        reviewed_by: String::new(),
        review_reference: String::new(),
        before_restore_sql: String::new(),
        after_restore_sql: String::new(),
        exclude_toc: vec![],
        expected_database: expected
            .map(|i| i.target_database.clone())
            .unwrap_or(Value::Null),
        authorization_checks: vec![],
    })
}

pub fn supported_exclusion(entry: &str) -> bool {
    [
        "POLICY ",
        "FK CONSTRAINT ",
        "FUNCTION ",
        "PROCEDURE ",
        "TRIGGER ",
        "DEFAULT ",
        "CONSTRAINT ",
        "CHECK CONSTRAINT ",
        "VIEW ",
        "EXTENSION ",
        "COMMENT ",
    ]
    .iter()
    .any(|prefix| entry.starts_with(prefix))
        && entry.split_whitespace().collect::<Vec<_>>().join(" ") == entry
}

pub fn has_managed_reference(value: &Value) -> Result<bool> {
    let text = serde_json::to_string(value)?
        .to_lowercase()
        .replace(['"', '\\', ' ', '\n', '\r', '\t'], "");
    Ok([
        "auth.",
        "storage.",
        "vault.",
        "realtime.",
        "net.",
        "supabase_",
    ]
    .iter()
    .any(|s| text.contains(s)))
}

fn tables(value: &Value) -> Result<BTreeMap<(String, String), &Value>> {
    let mut result = BTreeMap::new();
    for t in value["tables"]
        .as_array()
        .context("missing table inventory")?
    {
        let key = (
            t["schema"]
                .as_str()
                .context("missing table schema")?
                .to_owned(),
            t["name"].as_str().context("missing table name")?.to_owned(),
        );
        ensure!(result.insert(key, t).is_none(), "duplicate table inventory");
    }
    Ok(result)
}

impl Adaptation {
    pub fn validate(&self, inventory: &Inventory) -> Result<()> {
        ensure!(
            self.version == 1 && self.source_schema_sha256 == source_digest(inventory)?,
            "adaptation belongs to a different source schema or version"
        );
        ensure!(
            [&self.reviewed_by, &self.review_reference]
                .iter()
                .all(|s| !s.trim().is_empty() && !s.contains("REPLACE_")),
            "adaptation requires an approver and review reference"
        );
        for key in SCHEMA_KEYS {
            ensure!(
                self.expected_database[key].is_array(),
                "expected destination schema inventory is incomplete"
            );
        }
        let expected_names = self.expected_database["schema_names"]
            .as_array()
            .context("missing expected schemas")?
            .iter()
            .map(|s| s.as_str().context("invalid expected schema name"))
            .collect::<Result<BTreeSet<_>>>()?;
        ensure!(
            expected_names
                == inventory
                    .config
                    .schemas
                    .iter()
                    .map(String::as_str)
                    .collect(),
            "expected schema allowlist differs from the source plan"
        );
        let source = tables(&inventory.database)?;
        let target = tables(&self.expected_database)?;
        ensure!(
            source.keys().eq(target.keys()),
            "adaptations must preserve the application table set"
        );
        for (key, original) in source {
            let expected = target[&key];
            ensure!(
                expected["relkind"] == original["relkind"],
                "adaptations cannot change table kinds"
            );
            let shape = |t: &Value| -> Result<Vec<Value>> {
                t["columns"]
                    .as_array()
                    .context("missing column inventory")?
                    .iter()
                    .map(|c| {
                        let mut c = c.as_object().context("invalid column inventory")?.clone();
                        if c.get("generated")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                        {
                            c.remove("default");
                        }
                        Ok(Value::Object(c))
                    })
                    .collect()
            };
            ensure!(
                shape(original)? == shape(expected)?,
                "adaptations cannot change data column names, order, types, or generated semantics"
            );
            ensure!(
                expected["rls"] == true,
                "adapted application tables must retain RLS"
            );
            ensure!(
                original["indexes"] == expected["indexes"],
                "adaptations must preserve application indexes"
            );
            let names = |v: &Value| -> Result<BTreeSet<String>> {
                if v.is_null() {
                    return Ok(BTreeSet::new());
                }
                v.as_array()
                    .context("invalid constraint inventory")?
                    .iter()
                    .map(|c| {
                        Ok(c["name"]
                            .as_str()
                            .context("missing constraint name")?
                            .to_owned())
                    })
                    .collect()
            };
            ensure!(
                names(&original["constraints"])? == names(&expected["constraints"])?,
                "adaptations must preserve named constraints"
            );
        }
        for key in [
            "tables",
            "views",
            "functions",
            "policies",
            "triggers",
            "types",
            "rules",
            "foreign_keys",
        ] {
            ensure!(
                !has_managed_reference(&self.expected_database[key])?,
                "expected destination schema still references a source managed service"
            );
        }
        let mut exclusions = BTreeSet::new();
        for f in self.expected_database["functions"]
            .as_array()
            .context("missing expected functions")?
        {
            if f["security_definer"] == true {
                ensure!(
                    f["definer_owner"]
                        .as_str()
                        .is_some_and(crate::config::identifier),
                    "definer functions require an explicit target owner"
                );
                ensure!(
                    f["settings"]
                        .as_array()
                        .is_some_and(|settings| settings.iter().any(|s| s.as_str().is_some_and(
                            |s| s.starts_with("search_path=") && !s.contains("$user")
                        ))),
                    "definer functions require a fixed search_path"
                );
            }
        }
        for entry in &self.exclude_toc {
            ensure!(
                supported_exclusion(entry),
                "only named schema entries may be excluded; table data and sequence state cannot be excluded"
            );
            ensure!(exclusions.insert(entry), "duplicate archive exclusion");
        }
        let mut names = BTreeSet::new();
        ensure!(
            !self.authorization_checks.is_empty(),
            "adaptations require read-only authorization checks"
        );
        for check in &self.authorization_checks {
            ensure!(
                !check.name.trim().is_empty()
                    && !check.sql.trim().is_empty()
                    && names.insert(&check.name),
                "authorization checks need unique names and SQL"
            );
            ensure!(
                crate::config::identifier(&check.role),
                "authorization checks require a simple target runtime role name"
            );
        }
        Ok(())
    }

    pub async fn verify(&self, connection: &Connection, schemas: &[String]) -> Result<()> {
        let actual = crate::database::inventory(connection, schemas).await?;
        for key in SCHEMA_KEYS {
            ensure!(
                actual[key] == self.expected_database[key],
                "adapted destination schema differs from the reviewed expectation"
            );
        }
        for f in self.expected_database["functions"]
            .as_array()
            .context("missing expected functions")?
        {
            if f["security_definer"] == true {
                let owner = f["definer_owner"]
                    .as_str()
                    .context("missing definer owner")?;
                let safe=connection.query(&format!("SELECT to_json(COALESCE((SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolbypassrls FROM pg_roles WHERE rolname='{owner}'),false));")).await?;
                ensure!(
                    safe == true,
                    "definer owner must be a non-login role without superuser or RLS bypass privileges"
                );
            }
        }
        for check in &self.authorization_checks {
            let schemas = schemas
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(",");
            let safe=connection.query(&format!("SELECT to_json(COALESCE((SELECT NOT r.rolsuper AND NOT r.rolbypassrls AND NOT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=ANY(ARRAY[{schemas}]::text[]) AND pg_has_role(r.oid,c.relowner,'USAGE')) FROM pg_roles r WHERE r.rolname='{}'),false));",check.role)).await?;
            ensure!(
                safe == true,
                "authorization check role is missing, owns application objects, or can bypass RLS"
            );
            let actor = check.actor_id.map(|id| id.to_string()).unwrap_or_default();
            let sql = format!(
                "BEGIN READ ONLY; SET LOCAL ROLE \"{}\"; SET LOCAL app.actor_id = '{actor}';\n{}\n;ROLLBACK;",
                check.role, check.sql
            );
            ensure!(
                connection.query(&sql).await? == check.expected,
                "reviewed authorization check failed"
            );
        }
        Ok(())
    }
}
