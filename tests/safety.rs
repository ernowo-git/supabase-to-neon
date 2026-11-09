use anyhow::Result;
use serde_json::json;
use std::collections::BTreeMap;
use supabase_to_neon::{
    adaptation::{self, AuthorizationCheck},
    auth::{self, Enrollment, Mapping},
    config::Config,
    database,
    plan::{self, Inventory},
    read_json,
    references::{PreviewSpec, Rewrite},
    run::Run,
    write_private,
};
use uuid::Uuid;

fn fixture() -> Inventory {
    let config: Config = toml::from_str(include_str!("../examples/migration.toml")).unwrap();
    let db = json!({"routine_grants":[],"server_version":"170000","schema_names":["public"],"types":[],"rules":[],"tables":[],"views":[],"functions":[],"policies":[],"triggers":[],"extensions":[{"name":"plpgsql"}],"foreign_keys":[],"sequences":[],"grants":[],"publications":[],"foreign_servers":[]});
    Inventory {
        version: 2,
        config,
        source_identity: "source".into(),
        target_identity: "target".into(),
        tools: BTreeMap::from([
            ("psql".into(), "psql (PostgreSQL) 17.6".into()),
            ("pg_dump".into(), "pg_dump (PostgreSQL) 17.6".into()),
            ("pg_restore".into(), "pg_restore (PostgreSQL) 17.6".into()),
        ]),
        database: db.clone(),
        target_database: db,
        auth: json!({"users":[],"providers":[],"mfa_factors":0}),
        buckets: vec![vec![]],
        source_buckets: json!([{"name":"documents","public":false,"object_count":0}]),
    }
}

#[test]
fn url_preview_requires_an_allowlisted_text_column_and_unsigned_prefixes() -> Result<()> {
    let mut i = fixture();
    i.database["tables"] = json!([{"schema":"public","name":"parts","columns":[{"name":"image_url","type":"text","generated":""}]}]);
    let p = plan::build(i)?;
    let mut spec = PreviewSpec {
        version: 1,
        plan_digest: p.digest.clone(),
        rewrites: vec![Rewrite {
            schema: "public".into(),
            table: "parts".into(),
            column: "image_url".into(),
            source_prefix: "https://old.example/media/".into(),
            target_prefix: "https://new.example/media/".into(),
        }],
    };
    spec.validate(&p)?;
    spec.rewrites[0].source_prefix = "https://old.example/media/?token=secret".into();
    assert!(spec.validate(&p).is_err());
    spec.rewrites[0].source_prefix = "https://old.example/media/".into();
    spec.rewrites[0].column = "arbitrary_column".into();
    assert!(spec.validate(&p).is_err());
    spec.rewrites[0].column = "image_url".into();
    spec.plan_digest = "different".into();
    assert!(spec.validate(&p).is_err());
    Ok(())
}

#[test]
fn reviewed_adaptation_is_source_bound_and_preserves_data_shape() -> Result<()> {
    let mut i = fixture();
    i.database["tables"] = json!([{"schema":"public","name":"notes","relkind":"r","rls":true,"columns":[{"name":"id","type":"uuid","not_null":true,"generated":"","default":null}],"constraints":null,"indexes":null}]);
    let mut a = adaptation::template(&i, None)?;
    a.expected_database = i.database.clone();
    a.reviewed_by = "reviewer".into();
    a.review_reference = "review-123".into();
    a.authorization_checks.push(AuthorizationCheck {
        name: "missing actor".into(),
        role: "app_reader".into(),
        actor_id: None,
        sql: "SELECT to_json(0);".into(),
        expected: json!(0),
    });
    a.validate(&i)?;
    let p = plan::build_with_adaptation(i.clone(), Some(a.clone()))?;
    p.validate()?;
    let mut tampered = p;
    tampered.adaptation.as_mut().unwrap().before_restore_sql = "SELECT 1;".into();
    assert!(tampered.validate().is_err());
    let mut wrong = a.clone();
    wrong.expected_database["tables"][0]["columns"][0]["type"] = json!("text");
    assert!(wrong.validate(&i).is_err());
    let mut wrong = a.clone();
    wrong.expected_database["tables"][0]["rls"] = json!(false);
    assert!(wrong.validate(&i).is_err());
    let mut wrong = a.clone();
    wrong.source_schema_sha256 = "stale".into();
    assert!(wrong.validate(&i).is_err());
    let mut wrong = a.clone();
    wrong.expected_database["schema_names"] = json!(["unexpected"]);
    assert!(wrong.validate(&i).is_err());
    let mut wrong = a;
    wrong.authorization_checks.clear();
    assert!(wrong.validate(&i).is_err());
    Ok(())
}

#[test]
fn archive_filter_never_excludes_data_and_requires_exact_unique_matches() -> Result<()> {
    let toc = "; archive\n5; 2615 2200 SCHEMA - public postgres\n10; 0 123 TABLE DATA public notes postgres\n11; 3256 456 POLICY public notes owner_only postgres\n";
    let exclusions = vec!["POLICY public notes owner_only postgres".into()];
    let result = database::filter_toc(toc, &["public".into()], &exclusions)?;
    assert!(result.contains("; 11;"));
    assert!(result.contains("\n10; 0 123 TABLE DATA"));
    assert!(database::filter_toc(toc, &[], &["TABLE DATA public notes postgres".into()]).is_err());
    assert!(
        database::filter_toc(toc, &[], &["POLICY public notes missing postgres".into()]).is_err()
    );
    assert!(
        database::filter_toc(
            &(toc.to_owned() + "12; 3256 789 POLICY public notes owner_only postgres\n"),
            &[],
            &exclusions
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn deterministic_plan_binds_inventory_and_rejects_tampering() -> Result<()> {
    let a = plan::build(fixture())?;
    let b = plan::build(fixture())?;
    assert!(a.blockers.is_empty());
    assert_eq!(a.digest, b.digest);
    a.validate()?;
    let mut modified = a.clone();
    modified.inventory.target_identity = "different".into();
    assert!(modified.validate().is_err());
    let mut modified = a;
    modified.blockers.push("inserted".into());
    assert!(modified.validate().is_err());
    Ok(())
}

#[test]
fn unsafe_dependencies_and_missing_inventory_block_apply() -> Result<()> {
    let mut i = fixture();
    i.database["foreign_keys"] = json!([{"target_schema":"auth"}]);
    assert!(!plan::build(i)?.blockers.is_empty());
    let mut i = fixture();
    i.database["functions"] = json!([{"definition":"SELECT auth.uid()"}]);
    assert!(!plan::build(i)?.blockers.is_empty());
    let mut i = fixture();
    i.source_buckets = json!([{"name":"unmapped","public":true}]);
    assert!(!plan::build(i)?.blockers.is_empty());
    let mut i = fixture();
    i.database["policies"] = serde_json::Value::Null;
    assert!(plan::build(i).is_err());
    Ok(())
}

#[test]
fn config_rejects_remote_plaintext_managed_schemas_and_duplicate_targets() {
    let mut c = fixture().config;
    c.schemas = vec!["auth".into()];
    assert!(c.validate().is_err());
    let mut c = fixture().config;
    c.buckets[0].target.endpoint = "http://remote.example".into();
    assert!(c.validate().is_err());
    let mut c = fixture().config;
    c.buckets[0].target.endpoint = "https://key:secret@example.invalid".into();
    assert!(c.validate().is_err());
    let mut c = fixture().config;
    c.buckets.push(c.buckets[0].clone());
    assert!(c.validate().is_err());
}

#[test]
fn private_artifacts_do_not_overwrite_and_run_lock_binds_plan() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let file = temp.path().join("artifact");
    write_private(&file, b"original")?;
    assert!(write_private(&file, b"overwrite").is_err());
    assert_eq!(std::fs::read(file)?, b"original");
    let dir = temp.path().join("run");
    let run = Run::open(&dir, "plan-one")?;
    assert!(Run::open(&dir, "plan-one").is_err());
    run.event("database", "started", json!({}))?;
    assert_eq!(run.events()?.len(), 1);
    drop(run);
    assert!(Run::open(&dir, "plan-two").is_err());
    assert_eq!(read_json::<String>(&dir.join("binding.json"))?, "plan-one");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir)?.permissions().mode() & 0o777, 0o700);
    }
    Ok(())
}

#[test]
fn identity_mapping_requires_proof_unique_subjects_and_complete_enrollment() -> Result<()> {
    let id = Uuid::new_v4();
    let source = json!({"users":[{"id":id,"email":"same@example.test","disabled":false}]});
    let targets = json!([{"id":"neon-id","disabled":false}]);
    let mut enrollment = Enrollment {
        version: 1,
        plan_digest: "plan".into(),
        mappings: vec![],
    };
    assert!(auth::validate(&enrollment, &source, "plan", &targets).is_err());
    enrollment.mappings.push(Mapping {
        source_id: id,
        target_subject: "neon-id".into(),
        approved_by: "operator".into(),
        proof_reference: "verified-enrollment-1".into(),
    });
    auth::validate(&enrollment, &source, "plan", &targets)?;
    assert!(auth::validate(&enrollment, &source, "other-plan", &targets).is_err());
    enrollment.mappings[0].proof_reference.clear();
    assert!(auth::validate(&enrollment, &source, "plan", &targets).is_err());
    Ok(())
}

#[test]
fn cutover_evidence_rejects_stale_records_and_unedited_examples() -> Result<()> {
    let mut evidence: serde_json::Value =
        serde_json::from_str(include_str!("../examples/evidence.json"))?;
    assert!(plan::validate_evidence(&evidence, "plan", 1000).is_err());
    evidence["plan_digest"] = json!("plan");
    evidence["recorded_at_unix"] = json!(1000);
    assert!(plan::validate_evidence(&evidence, "plan", 1000).is_err());
    for (key, value) in evidence.as_object_mut().unwrap() {
        if key != "plan_digest" && key != "recorded_at_unix" {
            *value = json!("review-record-123");
        }
    }
    plan::validate_evidence(&evidence, "plan", 1000)?;
    assert!(plan::validate_evidence(&evidence, "plan", 5000).is_err());
    assert!(plan::validate_evidence(&evidence, "plan", 999).is_err());
    Ok(())
}
