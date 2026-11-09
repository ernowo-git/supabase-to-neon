use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use supabase_to_neon::{
    auth,
    config::Config,
    database, digest,
    plan::{self, Plan},
    read_json,
    run::Run,
    storage, write_json, write_private,
};

#[derive(Parser)]
#[command(
    version,
    about = "Inspect and rehearse a Supabase to Neon migration. Alpha: review compatibility.md before use."
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Init {
        #[arg(long, default_value = "migration.toml")]
        out: PathBuf,
    },
    Doctor {
        #[arg(long, default_value = "migration.toml")]
        config: PathBuf,
    },
    Inspect {
        #[arg(long, default_value = "migration.toml")]
        config: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Plan {
        #[arg(long)]
        inventory: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    AuthExport {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Apply {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, value_enum)]
        phase: Phase,
        #[arg(long)]
        run: PathBuf,
        #[arg(long)]
        ack_plan: Option<String>,
        #[arg(long)]
        writes_frozen: bool,
        #[arg(long)]
        enrollment: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    Verify {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        enrollment: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
    CutoverCheck {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        enrollment: Option<PathBuf>,
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Report {
        #[arg(long)]
        run: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Phase {
    Prepare,
    Database,
    Auth,
    Storage,
}
impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Database => "database",
            Self::Auth => "auth",
            Self::Storage => "storage",
        }
    }
}

fn load_plan(path: &Path) -> Result<Plan> {
    let plan: Plan = read_json(path)?;
    plan.validate()?;
    Ok(plan)
}
fn print(value: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let (result, code) = tokio::select! {
        result = execute(cli.command) => (result,1),
        _ = tokio::signal::ctrl_c() => (Err(anyhow::anyhow!("interrupted; inspect the run journal before resuming")),130),
    };
    if let Err(error) = result {
        eprintln!("{}", json!({"ok":false,"error":error.to_string()}));
        std::process::exit(code);
    }
}

async fn execute(action: Action) -> Result<()> {
    match action {
        Action::Init { out } => {
            write_private(&out, include_bytes!("../examples/migration.toml"))?;
            print(json!({"ok":true,"config":out}))
        }
        Action::Doctor { config } => {
            let cfg = Config::load(&config)?;
            let source = database::Connection::load(&cfg.source)?;
            let target = database::Connection::load(&cfg.target)?;
            database::assert_distinct(&source, &target)?;
            let tools = database::tools().await?;
            let sql = "SELECT json_build_object('version',current_setting('server_version_num'),'database',current_database(),'read_only',current_setting('transaction_read_only'));";
            let a = source.query(sql).await?;
            let b = target.query(sql).await?;
            for bucket in &cfg.buckets {
                storage::inventory(&storage::connect(&bucket.source)?).await?;
                storage::inventory(&storage::connect(&bucket.target)?).await?;
            }
            print(
                json!({"ok":true,"tools":tools,"source":a,"target":b,"note":"Connectivity checks do not establish migration readiness or storage access policy correctness."}),
            )
        }
        Action::Inspect { config, out } => {
            let inventory = plan::inspect(Config::load(&config)?).await?;
            write_json(&out, &inventory)?;
            print(json!({"ok":true,"inventory":out}))
        }
        Action::Plan { inventory, out } => {
            let plan = plan::build(read_json(&inventory)?)?;
            write_json(&out, &plan)?;
            print(
                json!({"ok":plan.blockers.is_empty(),"digest":plan.digest,"blockers":plan.blockers,"plan":out}),
            )?;
            ensure!(
                plan.blockers.is_empty(),
                "plan contains blockers; review the saved plan"
            );
            Ok(())
        }
        Action::AuthExport { plan, out } => {
            let p = load_plan(&plan)?;
            auth::export(&p.inventory.auth, &p.digest, &out)?;
            print(json!({"ok":true,"enrollment_manifest":out,"provider_accounts_created":false}))
        }
        Action::Apply {
            plan,
            phase,
            run,
            ack_plan,
            writes_frozen,
            enrollment,
            dry_run,
        } => {
            let p = load_plan(&plan)?;
            ensure!(
                p.blockers.is_empty(),
                "plan contains blockers; no mutation is allowed"
            );
            if dry_run {
                return print(
                    json!({"ok":true,"dry_run":true,"phase":phase.name(),"plan_digest":p.digest,"manual_actions":p.manual_actions}),
                );
            }
            ensure!(
                ack_plan.as_deref() == Some(p.digest.as_str()),
                "supply --ack-plan with the reviewed plan digest"
            );
            ensure!(
                writes_frozen,
                "freeze database, Auth, storage, and background writes, then supply --writes-frozen"
            );
            let (source, target) = p.connections().await?;
            let r = Run::open(&run, &p.digest)?;
            r.event(
                phase.name(),
                "started",
                json!({"writes_frozen_acknowledged":true}),
            )?;
            let result = apply(&p, phase, &r, &source, &target, enrollment.as_deref()).await;
            match result {
                Ok(detail) => {
                    r.event(phase.name(), "completed", detail)?;
                    print(json!({"ok":true,"phase":phase.name(),"run":run}))
                }
                Err(error) => {
                    r.event(
                        phase.name(),
                        "failed",
                        json!({"message":"phase did not complete; inspect target before retrying"}),
                    )?;
                    Err(error)
                }
            }
        }
        Action::Verify {
            plan,
            enrollment,
            out,
        } => {
            let p = load_plan(&plan)?;
            let report = verify(&p, enrollment.as_deref()).await?;
            write_json(&out, &report)?;
            print(report)
        }
        Action::CutoverCheck {
            plan,
            enrollment,
            evidence,
            out,
        } => {
            let p = load_plan(&plan)?;
            let evidence: Value = read_json(&evidence)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            plan::validate_evidence(&evidence, &p.digest, now)?;
            let mut report = verify(&p, enrollment.as_deref()).await?;
            report["operator_evidence_sha256"] = json!(digest(&serde_json::to_vec(&evidence)?));
            report["readiness"] = json!(
                "technical checks passed with operator-attested application evidence; traffic was not switched"
            );
            report["cutover_ready"] = json!(true);
            write_json(&out, &report)?;
            print(report)
        }
        Action::Report { run } => {
            let binding: String = read_json(&run.join("binding.json"))?;
            let r = Run::open(&run, &binding)?;
            print(json!({"ok":true,"plan_digest":binding,"events":r.events()?}))
        }
    }
}

async fn apply(
    p: &Plan,
    phase: Phase,
    run: &Run,
    source: &database::Connection,
    target: &database::Connection,
    enrollment: Option<&Path>,
) -> Result<Value> {
    match phase {
        Phase::Prepare => {
            let actual = database::inventory(target, &p.inventory.config.schemas).await?;
            ensure!(
                actual == p.inventory.target_database,
                "target schema drift since planning"
            );
            for b in &p.inventory.config.buckets {
                ensure!(
                    storage::inventory(&storage::connect(&b.target)?)
                        .await?
                        .is_empty(),
                    "initial destination bucket must be empty"
                );
            }
            Ok(
                json!({"target_preflight":true,"note":"Provision Auth and bucket visibility through the provider before transfer."}),
            )
        }
        Phase::Database => {
            let actual = database::inventory(target, &p.inventory.config.schemas).await?;
            ensure!(
                actual == p.inventory.target_database,
                "target application schema is no longer the empty planned baseline; use a fresh target after partial restore"
            );
            let archive = run.directory.join("database.dump");
            ensure!(
                !archive.exists(),
                "this run already has an archive; use a new run and a verified empty target"
            );
            let before = table_digests(p, source).await?;
            source.dump(&p.inventory.config.schemas, &archive).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o600))?;
            }
            ensure!(
                table_digests(p, source).await? == before,
                "source data changed around dump; freeze writes and restart"
            );
            let existing_schemas: Vec<String> = p.inventory.target_database["schema_names"]
                .as_array()
                .context("missing target schema inventory")?
                .iter()
                .map(|s| {
                    s.as_str()
                        .map(String::from)
                        .context("invalid target schema name")
                })
                .collect::<Result<_>>()?;
            target.restore(&archive, &existing_schemas).await?;
            ensure!(
                table_digests(p, target).await? == before,
                "restored rows differ from frozen source"
            );
            verify_schema(p, target).await?;
            verify_sequences(p, source, target).await?;
            Ok(json!({"tables":before,"archive_sha256":file_digest(&archive)?}))
        }
        Phase::Auth => {
            let enrollment: auth::Enrollment = read_json(
                enrollment.context("supply --enrollment with the approved mapping file")?,
            )?;
            auth::validate(
                &enrollment,
                &p.inventory.auth,
                &p.digest,
                &auth::target_users(target).await?,
            )?;
            let present = target
                .query("SELECT to_json(to_regnamespace('migration_identity') IS NOT NULL);")
                .await?;
            if present != true {
                let sql = auth::sql(&enrollment, &p.inventory.auth)?;
                let path = run.directory.join("principals.sql");
                if !path.exists() {
                    write_private(&path, sql.as_bytes())?;
                }
                ensure!(
                    std::fs::read(&path)? == sql.as_bytes(),
                    "saved principal SQL differs"
                );
                target.execute_file(&path).await?;
            }
            auth::verify(target, &enrollment, &p.inventory.auth, &p.digest).await?;
            Ok(
                json!({"principals_verified":true,"enrollment_sha256":digest(&serde_json::to_vec(&enrollment)?)}),
            )
        }
        Phase::Storage => {
            for (i, b) in p.inventory.config.buckets.iter().enumerate() {
                storage::transfer(
                    &storage::connect(&b.source)?,
                    &storage::connect(&b.target)?,
                    &p.inventory.buckets[i],
                    run,
                    i,
                )
                .await?;
            }
            Ok(
                json!({"buckets_verified":p.inventory.config.buckets.len(),"access_policy_verification":"requires operator evidence"}),
            )
        }
    }
}

fn file_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

async fn table_digests(
    p: &Plan,
    conn: &database::Connection,
) -> Result<BTreeMap<String, database::TableDigest>> {
    let mut result = BTreeMap::new();
    for t in p.inventory.database["tables"]
        .as_array()
        .context("missing table inventory")?
    {
        let schema = t["schema"].as_str().context("missing table schema")?;
        let name = t["name"].as_str().context("missing table name")?;
        result.insert(
            format!("{schema}.{name}"),
            conn.table_digest(schema, name).await?,
        );
    }
    Ok(result)
}

async fn verify_schema(p: &Plan, target: &database::Connection) -> Result<()> {
    let actual = database::inventory(target, &p.inventory.config.schemas).await?;
    for key in [
        "tables",
        "views",
        "functions",
        "policies",
        "triggers",
        "sequences",
        "foreign_keys",
        "types",
        "rules",
    ] {
        ensure!(
            actual[key] == p.inventory.database[key],
            "restored schema differs from source"
        );
    }
    // pg_dump restores sequence values independently of schema definitions.
    Ok(())
}

async fn verify_sequences(
    p: &Plan,
    source: &database::Connection,
    target: &database::Connection,
) -> Result<()> {
    for s in p.inventory.database["sequences"]
        .as_array()
        .context("missing sequences")?
    {
        let schema = s["schema"].as_str().context("missing sequence schema")?;
        let name = s["name"].as_str().context("missing sequence name")?;
        ensure!(
            source.sequence_state(schema, name).await?
                == target.sequence_state(schema, name).await?,
            "sequence state differs"
        );
    }
    Ok(())
}

async fn verify(p: &Plan, enrollment: Option<&Path>) -> Result<Value> {
    ensure!(p.blockers.is_empty(), "plan contains unresolved blockers");
    let (source, target) = p.connections().await?;
    verify_schema(p, &target).await?;
    verify_sequences(p, &source, &target).await?;
    let a = table_digests(p, &source).await?;
    ensure!(
        a == table_digests(p, &target).await?,
        "database rows differ"
    );
    let enrollment: auth::Enrollment =
        read_json(enrollment.context("supply --enrollment to verify migrated identities")?)?;
    auth::verify(&target, &enrollment, &p.inventory.auth, &p.digest).await?;
    for (i, b) in p.inventory.config.buckets.iter().enumerate() {
        storage::verify(
            &storage::connect(&b.source)?,
            &storage::connect(&b.target)?,
            &p.inventory.buckets[i],
        )
        .await?;
    }
    ensure!(
        table_digests(p, &source).await? == a,
        "source changed during verification"
    );
    Ok(
        json!({"ok":true,"plan_digest":p.digest,"database_tables":a,"auth":"principal mappings verified; login flows require external evidence","storage":"all configured object bytes and metadata verified","cutover_ready":false,"note":"Run cutover-check with fresh operator evidence before switching traffic."}),
    )
}
