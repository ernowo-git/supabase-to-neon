use crate::{
    config::{Database, identifier, secret},
    digest,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use url::Url;

pub struct Connection {
    env: BTreeMap<String, String>,
    _password: tempfile::NamedTempFile,
    pub identity: String,
}

impl Connection {
    pub fn load(database: &Database) -> Result<Self> {
        let raw = secret(&database.connection_env)?;
        let url = Url::parse(&raw).map_err(|_| anyhow::anyhow!("invalid database URL"))?;
        ensure!(
            matches!(url.scheme(), "postgres" | "postgresql"),
            "expected a PostgreSQL URL"
        );
        let host = url.host_str().context("database host is required")?;
        ensure!(
            !host.contains("pooler") && !host.contains("-pooler"),
            "use a direct unpooled database endpoint"
        );
        let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1");
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        ensure!(
            params
                .keys()
                .all(|k| matches!(k.as_str(), "sslmode" | "sslrootcert")),
            "database URL accepts only sslmode and sslrootcert options"
        );
        let mode = params
            .get("sslmode")
            .map(String::as_str)
            .unwrap_or("verify-full");
        ensure!(
            mode == "verify-full" || (local && mode == "disable"),
            "remote databases require sslmode=verify-full"
        );
        let user = decode(url.username())?;
        let password = decode(url.password().unwrap_or(""))?;
        let name = decode(url.path().trim_start_matches('/'))?;
        ensure!(
            !name.is_empty() && !user.is_empty() && !name.contains('/'),
            "database name and user are required"
        );
        ensure!(
            ![host, &user, &password, &name]
                .iter()
                .any(|s| s.contains(['\n', '\r', '\0'])),
            "invalid database connection characters"
        );
        let port = url.port().unwrap_or(5432).to_string();
        let mut passfile = tempfile::NamedTempFile::new()?;
        let escape = |s: &str| s.replace('\\', "\\\\").replace(':', "\\:");
        writeln!(
            passfile,
            "{}:{}:{}:{}:{}",
            escape(host),
            port,
            escape(&name),
            escape(&user),
            escape(&password)
        )?;
        passfile.as_file().sync_all()?;
        let identity = digest(format!("{host}:{port}/{name}").as_bytes());
        let mut env = BTreeMap::from([
            ("PGHOST".into(), host.into()),
            ("PGPORT".into(), port),
            ("PGDATABASE".into(), name),
            ("PGUSER".into(), user),
            ("PGSSLMODE".into(), mode.into()),
            ("PGCONNECT_TIMEOUT".into(), "15".into()),
            (
                "PGPASSFILE".into(),
                passfile.path().to_string_lossy().into_owned(),
            ),
            (
                "PGSSLROOTCERT".into(),
                params
                    .get("sslrootcert")
                    .cloned()
                    .unwrap_or("system".into()),
            ),
            ("PGAPPNAME".into(), "supabase-to-neon".into()),
        ]);
        if mode == "disable" {
            env.remove("PGSSLROOTCERT");
        }
        Ok(Self {
            env,
            _password: passfile,
            identity,
        })
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .envs(&self.env)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        cmd.kill_on_drop(true).stderr(Stdio::null());
        cmd
    }

    pub async fn query(&self, sql: &str) -> Result<Value> {
        let mut child = self
            .command("psql")
            .args(["-X", "-w", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1"])
            .env(
                "PGOPTIONS",
                "-c default_transaction_read_only=on -c statement_timeout=120000",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("cannot start psql")?;
        child
            .stdin
            .take()
            .context("missing psql stdin")?
            .write_all(sql.as_bytes())
            .await?;
        let output = child.wait_with_output().await?;
        ensure!(
            output.status.success(),
            "database inspection failed; check connectivity, TLS, privileges, and supported schema version"
        );
        serde_json::from_slice(&output.stdout).context("database query returned invalid JSON")
    }

    pub async fn execute_file(&self, path: &std::path::Path) -> Result<()> {
        let result = self
            .command("psql")
            .args([
                "-X",
                "-w",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "--single-transaction",
                "-f",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .await?;
        ensure!(
            result.success(),
            "reviewed SQL failed; target requires inspection"
        );
        Ok(())
    }

    pub async fn dump(&self, schemas: &[String], path: &std::path::Path) -> Result<()> {
        self.dump_options(schemas, path, false).await
    }

    pub async fn dump_schema(&self, schemas: &[String], path: &std::path::Path) -> Result<()> {
        self.dump_options(schemas, path, true).await
    }

    async fn dump_options(
        &self,
        schemas: &[String],
        path: &std::path::Path,
        schema_only: bool,
    ) -> Result<()> {
        let mut cmd = self.command("pg_dump");
        cmd.args([
            "-w",
            "--format=custom",
            "--no-owner",
            "--no-privileges",
            "--file",
        ])
        .arg(path);
        if schema_only {
            cmd.arg("--schema-only");
        }
        for s in schemas {
            ensure!(identifier(s), "invalid schema");
            cmd.arg(format!("--schema={s}"));
        }
        ensure!(
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .await?
                .success(),
            "pg_dump failed"
        );
        Ok(())
    }

    pub async fn archive_list(&self, path: &std::path::Path) -> Result<String> {
        let output = self
            .command("pg_restore")
            .arg("--list")
            .arg(path)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "cannot inspect archive table of contents"
        );
        String::from_utf8(output.stdout).context("invalid archive table of contents")
    }

    pub async fn restore(
        &self,
        path: &std::path::Path,
        existing_schemas: &[String],
        exclusions: &[String],
    ) -> Result<()> {
        let toc = self.archive_list(path).await?;
        let mut list = tempfile::NamedTempFile::new()?;
        list.write_all(filter_toc(&toc, existing_schemas, exclusions)?.as_bytes())?;
        list.as_file().sync_all()?;
        let status = self
            .command("pg_restore")
            .args([
                "-w",
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
                "--single-transaction",
                "--dbname",
            ])
            .arg(self.env.get("PGDATABASE").context("missing database")?)
            .arg("--use-list")
            .arg(list.path())
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .await?;
        ensure!(
            status.success(),
            "restore failed; use a fresh isolated target before retrying"
        );
        Ok(())
    }

    pub async fn table_digest(&self, schema: &str, table: &str) -> Result<TableDigest> {
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        // Sorting canonical rows includes duplicates and works for tables without primary keys.
        // ponytail: PostgreSQL sorts the full table; use keyset external merging if rehearsal costs exceed the window.
        let sql = format!(
            "COPY (SELECT row_to_json(t)::text FROM {}.{} t ORDER BY row_to_json(t)::text COLLATE \"C\") TO STDOUT;",
            quote(schema),
            quote(table)
        );
        let mut child = self.command("psql").args(["-X","-w","-A","-t","-q","-v","ON_ERROR_STOP=1"])
            .env("PGOPTIONS", "-c default_transaction_read_only=on -c timezone=UTC -c datestyle=ISO -c extra_float_digits=3")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        child
            .stdin
            .take()
            .context("missing stdin")?
            .write_all(sql.as_bytes())
            .await?;
        let mut lines = BufReader::new(child.stdout.take().context("missing stdout")?).lines();
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let mut rows = 0;
        while let Some(line) = lines.next_line().await? {
            hash.update(line.as_bytes());
            hash.update(b"\n");
            rows += 1;
        }
        ensure!(child.wait().await?.success(), "table verification failed");
        Ok(TableDigest {
            rows,
            sha256: format!("{:x}", hash.finalize()),
        })
    }

    pub async fn sequence_state(&self, schema: &str, name: &str) -> Result<Value> {
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        self.query(&format!(
            "SELECT json_build_object('last_value',last_value,'is_called',is_called) FROM {}.{};",
            quote(schema),
            quote(name)
        ))
        .await
    }
}

pub fn filter_toc(toc: &str, existing_schemas: &[String], exclusions: &[String]) -> Result<String> {
    let mut matches = vec![0usize; exclusions.len()];
    let mut result = String::new();
    for line in toc.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let entry = fields.iter().skip(3).copied().collect::<Vec<_>>().join(" ");
        let is_entry = fields
            .first()
            .is_some_and(|f| f.ends_with(';') && f[..f.len() - 1].parse::<u64>().is_ok());
        let existing = is_entry
            && fields.len() >= 6
            && fields[3] == "SCHEMA"
            && fields[4] == "-"
            && existing_schemas.iter().any(|s| s == fields[5]);
        let mut excluded = false;
        for (i, requested) in exclusions.iter().enumerate() {
            ensure!(
                crate::adaptation::supported_exclusion(requested),
                "unsafe archive exclusion"
            );
            if is_entry && &entry == requested {
                matches[i] += 1;
                excluded = true;
            }
        }
        if existing || excluded {
            result.push_str("; ");
        }
        result.push_str(line);
        result.push('\n');
    }
    ensure!(
        matches.iter().all(|n| *n == 1),
        "archive exclusions must match exactly one schema entry each"
    );
    Ok(result)
}

fn decode(s: &str) -> Result<String> {
    Ok(percent_encoding::percent_decode_str(s)
        .decode_utf8()
        .map_err(|_| anyhow::anyhow!("invalid database credential encoding"))?
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_decoding_preserves_reserved_characters() -> Result<()> {
        assert_eq!(decode("p%40ss%3Aword%26more+text")?, "p@ss:word&more+text");
        assert_eq!(decode("literal&value")?, "literal&value");
        assert!(decode("%FF").is_err());
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableDigest {
    pub rows: u64,
    pub sha256: String,
}

pub async fn tools() -> Result<BTreeMap<String, String>> {
    let mut versions = BTreeMap::new();
    for tool in ["psql", "pg_dump", "pg_restore"] {
        let output = Command::new(tool)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("install {tool}"))?;
        ensure!(
            output.status.success(),
            "cannot determine PostgreSQL tool version"
        );
        versions.insert(tool.into(), String::from_utf8(output.stdout)?.trim().into());
    }
    Ok(versions)
}

pub async fn inventory(conn: &Connection, schemas: &[String]) -> Result<Value> {
    ensure!(schemas.iter().all(|s| identifier(s)), "invalid schemas");
    let array = schemas
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql =
        include_str!("inventory.sql").replace("__SCHEMAS__", &format!("ARRAY[{array}]::text[]"));
    conn.query(&sql).await
}

pub async fn auth_inventory(conn: &Connection) -> Result<Value> {
    let present = conn.query("SELECT json_build_object('users',to_regclass('auth.users') IS NOT NULL,'identities',to_regclass('auth.identities') IS NOT NULL,'factors',to_regclass('auth.mfa_factors') IS NOT NULL);").await?;
    if present["users"] != true {
        return Ok(json!({"users": [], "providers": [], "mfa_factors": 0}));
    }
    ensure!(
        present["identities"] == true && present["factors"] == true,
        "unsupported source Auth schema; inventory cannot establish completeness"
    );
    conn.query("SELECT json_build_object('users',COALESCE((SELECT json_agg(json_build_object('id',id,'email',email,'email_verified',email_confirmed_at IS NOT NULL,'disabled',banned_until > now(),'phone_user',phone IS NOT NULL AND phone <> '') ORDER BY id) FROM auth.users),'[]'::json),'providers',COALESCE((SELECT json_agg(provider ORDER BY provider) FROM (SELECT DISTINCT provider FROM auth.identities) p),'[]'::json),'mfa_factors',(SELECT count(*) FROM auth.mfa_factors));").await
}

pub fn assert_distinct(source: &Connection, target: &Connection) -> Result<()> {
    if source.identity == target.identity {
        bail!("source and target resolve to the same database endpoint");
    }
    Ok(())
}
