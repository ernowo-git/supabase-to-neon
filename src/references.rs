use crate::{database::Connection, digest, plan::Plan};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use url::Url;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSpec {
    pub version: u32,
    pub plan_digest: String,
    pub rewrites: Vec<Rewrite>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rewrite {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub source_prefix: String,
    pub target_prefix: String,
}

fn prefix(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid URL prefix")?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    ensure!(
        url.host_str().is_some() && (url.scheme() == "https" || (local && url.scheme() == "http")),
        "remote URL prefixes require HTTPS"
    );
    ensure!(
        value.ends_with('/')
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "URL prefixes must end in a slash and cannot contain credentials, query strings, or fragments"
    );
    Ok(())
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn literal(value: &str) -> String {
    format!(
        "E'{}'",
        value.replace('\\', "\\\\").replace(char::from(39), "''")
    )
}

impl PreviewSpec {
    pub fn validate(&self, plan: &Plan) -> Result<()> {
        ensure!(
            self.version == 1 && self.plan_digest == plan.digest,
            "URL preview belongs to a different plan or version"
        );
        ensure!(
            !self.rewrites.is_empty(),
            "URL preview needs an explicit column allowlist"
        );
        let mut columns = BTreeSet::new();
        for rewrite in &self.rewrites {
            prefix(&rewrite.source_prefix)?;
            prefix(&rewrite.target_prefix)?;
            ensure!(
                rewrite.source_prefix != rewrite.target_prefix,
                "source and target URL prefixes must differ"
            );
            ensure!(
                columns.insert((&rewrite.schema, &rewrite.table, &rewrite.column)),
                "a column may have only one URL rewrite rule"
            );
            let table = plan.inventory.database["tables"]
                .as_array()
                .context("missing table inventory")?
                .iter()
                .find(|t| t["schema"] == rewrite.schema && t["name"] == rewrite.table)
                .context("URL preview table is outside the plan allowlist")?;
            let column = table["columns"]
                .as_array()
                .context("missing columns")?
                .iter()
                .find(|c| c["name"] == rewrite.column)
                .context("URL preview column does not exist")?;
            let ty = column["type"].as_str().context("missing column type")?;
            ensure!(
                ty == "text" || ty == "character varying" || ty.starts_with("character varying("),
                "URL previews support plain text columns only; JSON, arrays, and domains require separate review"
            );
            ensure!(
                column["generated"].as_str().is_none_or(str::is_empty),
                "URL preview cannot propose changing a generated column"
            );
        }
        Ok(())
    }
}

pub async fn preview(
    plan: &Plan,
    spec: &PreviewSpec,
    target: &Connection,
    limit: usize,
) -> Result<Value> {
    spec.validate(plan)?;
    ensure!(
        (1..=10000).contains(&limit),
        "URL preview limit must be between 1 and 10000 per column"
    );
    let mut results = vec![];
    for rule in &spec.rewrites {
        let keys=target.query(&format!("SELECT COALESCE(json_agg(a.attname ORDER BY k.ordinality),'[]'::json) FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(attnum,ordinality) JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum=k.attnum WHERE i.indisprimary AND n.nspname={} AND c.relname={};",literal(&rule.schema),literal(&rule.table))).await?;
        let keys = keys.as_array().context("invalid primary key inventory")?;
        ensure!(
            !keys.is_empty(),
            "URL previews require a primary key to identify proposed changes"
        );
        let key_sql = keys
            .iter()
            .map(|key| {
                let name = key.as_str().context("invalid primary key column")?;
                Ok(format!("{},t.{}", literal(name), identifier(name)))
            })
            .collect::<Result<Vec<_>>>()?
            .join(",");
        let column = identifier(&rule.column);
        let old = literal(&rule.source_prefix);
        let new = literal(&rule.target_prefix);
        let manual = format!("strpos(t.{column},'?')>0 OR strpos(t.{column},'#')>0");
        let sql = format!(
            "WITH matches AS MATERIALIZED (SELECT json_build_object({key_sql}) AS key,t.{column} AS old_url,CASE WHEN {manual} THEN NULL ELSE {new} || substring(t.{column} FROM char_length({old})+1) END AS new_url,CASE WHEN {manual} THEN 'query string or fragment requires manual review' ELSE NULL END AS manual_reason FROM {}.{} t WHERE left(t.{column},char_length({old})) COLLATE \"C\" = {old} COLLATE \"C\") SELECT json_build_object('total',(SELECT count(*) FROM matches),'manual_count',(SELECT count(*) FROM matches WHERE manual_reason IS NOT NULL),'rows',COALESCE((SELECT json_agg(x ORDER BY x.key::text COLLATE \"C\") FROM (SELECT * FROM matches ORDER BY key::text COLLATE \"C\" LIMIT {limit}) x),'[]'::json));",
            identifier(&rule.schema),
            identifier(&rule.table)
        );
        let result = target.query(&sql).await?;
        results.push(json!({"schema":rule.schema,"table":rule.table,"column":rule.column,"source_prefix":rule.source_prefix,"target_prefix":rule.target_prefix,"total":result["total"],"manual_count":result["manual_count"],"truncated":result["total"].as_u64().context("missing preview count")?>limit as u64,"rows":result["rows"]}));
    }
    Ok(
        json!({"version":1,"plan_digest":plan.digest,"spec_sha256":digest(&serde_json::to_vec(spec)?),"applied":false,"columns":results,"note":"Read-only proposals. Access policy, URL validity, signed links, and application compatibility still require review. Applying these changes later changes database content and needs separate reconciliation."}),
    )
}
