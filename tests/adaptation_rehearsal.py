"""Reviewed schema and RLS translation inside the disposable rehearsal cluster."""
import json
from pathlib import Path


def rehearse_adaptation(path, sql, cli, client, active, disabled):
    sql("source", f"""
        CREATE ROLE source_authenticated NOLOGIN;
        CREATE FUNCTION auth.uid() RETURNS uuid LANGUAGE sql STABLE AS $$ SELECT nullif(current_setting('request.actor',true),'')::uuid $$;
        CREATE SCHEMA extensions;
        CREATE EXTENSION pgcrypto WITH SCHEMA extensions;
        CREATE TABLE public.notes(id uuid PRIMARY KEY,owner_id uuid NOT NULL REFERENCES auth.users(id),body text NOT NULL);
        INSERT INTO public.notes VALUES ('aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa','{active}','active note'),('bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb','{disabled}','disabled note');
        ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;
        CREATE POLICY owner_only ON public.notes TO source_authenticated USING (owner_id=auth.uid());
        UPDATE public.parts SET name='https://old.example/storage/public/source/photos/%E9%83%A8%E5%93%81.svg' WHERE id=1;
        UPDATE public.parts SET name='https://old.example/storage/public/source/photo.svg?token=synthetic' WHERE id=2;
    """)
    before = """
        CREATE ROLE app_reader NOLOGIN NOSUPERUSER NOBYPASSRLS;
        CREATE ROLE app_definer NOLOGIN NOSUPERUSER NOBYPASSRLS;
        CREATE SCHEMA extensions;
        CREATE EXTENSION pgcrypto WITH SCHEMA extensions;
    """
    after = """
        ALTER TABLE public.notes ADD CONSTRAINT notes_owner_id_fkey FOREIGN KEY(owner_id) REFERENCES migration_identity.principal(id);
        GRANT USAGE ON SCHEMA public TO app_reader;
        GRANT SELECT ON public.notes TO app_reader;
        GRANT USAGE ON SCHEMA migration_identity TO app_definer;
        GRANT SELECT(id,disabled) ON migration_identity.principal TO app_definer;
        CREATE POLICY principal_definer ON migration_identity.principal TO app_definer USING (id=nullif(current_setting('app.actor_id',true),'')::uuid AND NOT disabled);
        CREATE FUNCTION public.actor_is_active() RETURNS boolean LANGUAGE sql STABLE SECURITY DEFINER SET search_path=pg_catalog AS $$ SELECT EXISTS(SELECT 1 FROM migration_identity.principal p WHERE p.id=nullif(current_setting('app.actor_id',true),'')::uuid AND NOT p.disabled) $$;
        ALTER FUNCTION public.actor_is_active() OWNER TO app_definer;
        REVOKE ALL ON FUNCTION public.actor_is_active() FROM PUBLIC;
        GRANT EXECUTE ON FUNCTION public.actor_is_active() TO app_reader;
        CREATE POLICY owner_only ON public.notes TO app_reader USING (owner_id=nullif(current_setting('app.actor_id',true),'')::uuid AND public.actor_is_active());
    """
    # Build an independent, empty destination schema to capture the reviewed expectation.
    sql("target", before)
    sql("target", "CREATE TABLE public.notes(id uuid PRIMARY KEY,owner_id uuid NOT NULL,body text NOT NULL); ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;" + after)
    cli("inspect", "--out", "expected-inventory.json")
    sql("target", "DROP POLICY principal_definer ON migration_identity.principal; DROP SCHEMA public CASCADE; CREATE SCHEMA public; DROP SCHEMA extensions CASCADE; DROP OWNED BY app_reader; DROP OWNED BY app_definer; DROP ROLE app_reader; DROP ROLE app_definer;")
    for item in client.list_objects_v2(Bucket="target").get("Contents", []):
        client.delete_object(Bucket="target",Key=item["Key"])
    cli("inspect", "--out", "managed-inventory.json")
    cli("plan", "--inventory", "managed-inventory.json", "--out", "managed-blocked.json", success=False)
    cli("archive-list", "--plan", "managed-blocked.json", "--out", "archive-list.txt")
    cli("adaptation-template", "--inventory", "managed-inventory.json", "--expected-inventory", "expected-inventory.json", "--out", "adaptation.json")
    adaptation = json.loads((path / "adaptation.json").read_text())
    adaptation.update(reviewed_by="synthetic-reviewer",review_reference="fixture-schema-review",before_restore_sql=before,after_restore_sql=after)
    entries = [" ".join(line.split()[3:]) for line in (path / "archive-list.txt").read_text().splitlines() if line and not line.startswith(";")]
    adaptation["exclude_toc"] = [entry for entry in entries if entry.startswith("POLICY public notes ") or entry.startswith("FK CONSTRAINT public notes ")]
    assert len(adaptation["exclude_toc"]) == 2
    adaptation["authorization_checks"] = [
        {"name":name,"role":"app_reader","actor_id":actor,"sql":"SELECT to_json(count(*)) FROM public.notes;","expected":count}
        for name,actor,count in [("active owner",active,1),("disabled owner",disabled,0),("no actor",None,0),("unrelated actor","cccccccc-cccc-4ccc-8ccc-cccccccccccc",0)]
    ]
    (path / "adaptation.json").write_text(json.dumps(adaptation))
    cli("plan", "--inventory", "managed-inventory.json", "--adaptation", "adaptation.json", "--out", "managed-plan.json")
    digest = json.loads((path / "managed-plan.json").read_text())["digest"]
    enrollment = json.loads((path / "enrollment.json").read_text())
    enrollment["plan_digest"] = digest
    (path / "managed-enrollment.json").write_text(json.dumps(enrollment))
    args=["--plan","managed-plan.json","--run","managed-run","--ack-plan",digest,"--writes-frozen"]
    cli("apply",*args,"--phase","prepare")
    cli("apply",*args,"--phase","auth","--enrollment","managed-enrollment.json")
    cli("apply",*args,"--phase","database")
    cli("apply",*args,"--phase","storage")
    cli("verify","--plan","managed-plan.json","--enrollment","managed-enrollment.json","--out","managed-verified.json")
    spec={"version":1,"plan_digest":digest,"rewrites":[{"schema":"public","table":"parts","column":"name","source_prefix":"https://old.example/storage/public/source/","target_prefix":"https://new.example/media/target/"}]}
    (path / "url-spec.json").write_text(json.dumps(spec))
    cli("url-preview","--plan","managed-plan.json","--spec","url-spec.json","--limit","1","--out","url-preview.json")
    preview=json.loads((path / "url-preview.json").read_text())
    assert preview["applied"] is False
    assert preview["columns"][0]["total"]==2
    assert preview["columns"][0]["truncated"] is True
    assert preview["columns"][0]["manual_count"]==1
    assert preview["columns"][0]["rows"][0]["key"]=={"id":1}
    assert preview["columns"][0]["rows"][0]["new_url"]=="https://new.example/media/target/photos/%E9%83%A8%E5%93%81.svg"
    cli("url-preview","--plan","managed-plan.json","--spec","url-spec.json","--out","full-url-preview.json")
    assert json.loads((path / "full-url-preview.json").read_text())["columns"][0]["rows"][1]["new_url"] is None
    assert "https://old.example/" in sql("target","SELECT name FROM public.parts WHERE id=1;")
    sql("target", "ALTER ROLE app_definer LOGIN;")
    cli("verify","--plan","managed-plan.json","--enrollment","managed-enrollment.json","--out","unsafe-definer.json",success=False)
    sql("target", "ALTER ROLE app_definer NOLOGIN; GRANT EXECUTE ON FUNCTION public.actor_is_active() TO PUBLIC;")
    cli("verify","--plan","managed-plan.json","--enrollment","managed-enrollment.json","--out","unsafe-execution-grant.json",success=False)
    sql("target", "REVOKE EXECUTE ON FUNCTION public.actor_is_active() FROM PUBLIC;")
    sql("target", "ALTER ROLE app_reader BYPASSRLS;")
    cli("verify","--plan","managed-plan.json","--enrollment","managed-enrollment.json","--out","unsafe-role.json",success=False)
    assert not (path / "unsafe-role.json").exists()
    sql("target", "ALTER ROLE app_reader NOBYPASSRLS; ALTER POLICY owner_only ON public.notes USING (true);")
    cli("verify","--plan","managed-plan.json","--enrollment","managed-enrollment.json","--out","changed-policy.json",success=False)
    print("PASS: reviewed Auth foreign-key and RLS translation, extension preparation, active/disabled/missing/unrelated actors, and bypass-role and policy-drift rejection.")
