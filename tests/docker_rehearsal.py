#!/usr/bin/env python3
"""Synthetic rehearsal. Requires Docker, Python boto3, and `just build`."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import uuid

import boto3
from botocore.config import Config as S3Config

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/supabase-to-neon"
DOCKER = shutil.which("docker")
PG_IMAGE = os.environ.get("REHEARSAL_POSTGRES_IMAGE", "postgres@sha256:b0f9560a2de083e2cc7382e75f808c7381a32852a7ec49117deedb300e552b24")
S3_IMAGE = os.environ.get("REHEARSAL_S3_IMAGE", "minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e")
ACTIVE = "11111111-1111-4111-8111-111111111111"
DISABLED = "22222222-2222-4222-8222-222222222222"


def docker(*args, **kwargs):
    return subprocess.run([DOCKER, *args], check=True, capture_output=True, **kwargs).stdout.decode().strip()


def main():
    if not BINARY.is_file() or DOCKER is None:
        raise SystemExit("Run just build and install Docker first.")
    suffix = uuid.uuid4().hex[:12]
    pg = "supabase-to-neon-test-pg-" + suffix
    s3 = "supabase-to-neon-test-s3-" + suffix
    with tempfile.TemporaryDirectory(prefix="supabase-to-neon-test-") as tmp:
        path = Path(tmp)
        try:
            docker("run", "-d", "--name", pg, "--network", "none", "--tmpfs", "/var/lib/postgresql/data",
                   "-e", "POSTGRES_PASSWORD=synthetic-test-only", "-v", f"{tmp}:{tmp}", PG_IMAGE)
            for _ in range(60):
                result = subprocess.run([DOCKER, "exec", pg, "pg_isready", "-U", "postgres"], capture_output=True)
                if result.returncode == 0:
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError("test PostgreSQL did not start")

            def sql(database, statement):
                return docker("exec", "-i", pg, "psql", "-U", "postgres", "-d", database, "-v", "ON_ERROR_STOP=1", input=statement.encode())

            sql("postgres", "CREATE DATABASE source; CREATE DATABASE target;")
            sql("source", f"""
                CREATE SCHEMA auth;
                CREATE TABLE auth.users(id uuid PRIMARY KEY,email text,email_confirmed_at timestamptz,banned_until timestamptz,phone text);
                CREATE TABLE auth.identities(provider text);
                CREATE TABLE auth.mfa_factors(id uuid);
                INSERT INTO auth.users VALUES('{ACTIVE}','active@example.test',now(),NULL,NULL),('{DISABLED}','disabled@example.test',now(),'infinity',NULL);
                INSERT INTO auth.identities VALUES('email');
                CREATE SCHEMA storage;
                CREATE TABLE storage.buckets(id text,name text,public boolean);
                INSERT INTO storage.buckets VALUES('source','source',false);
                CREATE TABLE storage.objects(bucket_id text);
                INSERT INTO storage.objects VALUES('source'),('source'),('source');
                CREATE TABLE public.parts(id bigserial PRIMARY KEY,name text NOT NULL,price_minor bigint NOT NULL,metadata jsonb,created_at timestamptz);
                ALTER TABLE public.parts ENABLE ROW LEVEL SECURITY;
                INSERT INTO public.parts(name,price_minor,metadata,created_at) VALUES ('部品',12500,'{{"fitment":"test"}}',now()),('quoted\\text',900,'{{}}',now());
                SELECT setval('public.parts_id_seq',100,true);
                CREATE TYPE public.part_state AS ENUM ('available','ordered');
                ALTER TABLE public.parts ADD COLUMN state public.part_state NOT NULL DEFAULT 'available';
                CREATE DOMAIN public.minor_amount AS bigint CHECK (VALUE >= 0);
                CREATE VIEW public.available_parts WITH (security_invoker=true) AS SELECT id,name FROM public.parts;
            """)
            sql("target", "CREATE SCHEMA neon_auth; CREATE TABLE neon_auth.\"user\"(id text PRIMARY KEY,banned boolean NOT NULL DEFAULT false); INSERT INTO neon_auth.\"user\"(id) VALUES('enrolled-fixture-user');")
            docker("run", "-d", "--name", s3, "--tmpfs", "/data", "-p", "127.0.0.1::9000",
                   "-e", "MINIO_ROOT_USER=synthetic-key", "-e", "MINIO_ROOT_PASSWORD=synthetic-test-secret", S3_IMAGE, "server", "/data")
            port = docker("port", s3, "9000/tcp").rsplit(":", 1)[1]
            endpoint = "http://127.0.0.1:" + port
            client = boto3.client("s3", endpoint_url=endpoint, region_name="us-east-1", aws_access_key_id="synthetic-key",
                                  aws_secret_access_key="synthetic-test-secret", config=S3Config(s3={"addressing_style": "path"}, retries={"max_attempts": 1}))
            for _ in range(60):
                try:
                    client.list_buckets()
                    break
                except Exception:
                    time.sleep(0.5)
            else:
                raise RuntimeError("test S3 did not start")
            for name in ["source", "target"]:
                client.create_bucket(Bucket=name)
            for key, content in [("photos/部品.svg", b"<svg/>"), ("large.bin", b"x" * (6 * 1024 * 1024)), ("empty", b"")]:
                client.put_object(Bucket="source", Key=key, Body=content, ContentType="image/svg+xml" if key.endswith("svg") else "application/octet-stream",
                                  CacheControl="max-age=3600", Metadata={"fixture": "synthetic"})

            wrappers = path / "bin"
            wrappers.mkdir()
            # PostgreSQL tools execute inside the disposable container. Only synthetic credentials are passed.
            for tool in ["psql", "pg_dump", "pg_restore"]:
                wrapper = wrappers / tool
                wrapper.write_text("#!/usr/bin/env python3\nimport os,subprocess,sys\ncmd=" + repr([DOCKER,"exec","-i","--user",f"{os.getuid()}:{os.getgid()}","-w",tmp]) + "\nfor k,v in os.environ.items():\n if k.startswith('PG'): cmd+=['-e',k+'='+v]\ncmd+=" + repr([pg,tool]) + "+sys.argv[1:]\nresult=subprocess.run(cmd,stderr=subprocess.PIPE)\nif result.returncode: open(" + repr(str(path / "tool-error.txt")) + ", 'wb').write(result.stderr)\nraise SystemExit(result.returncode)\n")
                wrapper.chmod(0o700)
            env = dict(os.environ, PATH=str(wrappers) + os.pathsep + os.environ["PATH"], TMPDIR=tmp,
                       SOURCE_DATABASE_URL="postgresql://postgres:synthetic-test-only@localhost/source?sslmode=disable",
                       TARGET_DATABASE_URL="postgresql://postgres:synthetic-test-only@localhost/target?sslmode=disable",
                       SOURCE_S3_ACCESS_KEY="synthetic-key", SOURCE_S3_SECRET_KEY="synthetic-test-secret",
                       TARGET_S3_ACCESS_KEY="synthetic-key", TARGET_S3_SECRET_KEY="synthetic-test-secret")
            config = (ROOT / "examples/migration.toml").read_text().replace("https://source-project.storage.supabase.co/storage/v1/s3", endpoint).replace("https://replace-with-neon-storage-endpoint.example", endpoint)
            config = config.replace('bucket = "documents"', 'bucket = "source"', 1)
            config = config.replace('bucket = "documents"', 'bucket = "target"', 1)
            config = config.replace('region = "ap-southeast-1"', 'region = "us-east-1"')
            (path / "migration.toml").write_text(config)

            def cli(*args, success=True):
                result = subprocess.run([str(BINARY), *args], cwd=path, env=env, capture_output=True, text=True)
                if success and result.returncode != 0:
                    diagnostic = (path / "tool-error.txt").read_text() if (path / "tool-error.txt").exists() else ""
                    raise RuntimeError("CLI failed: " + " ".join(args[:2]) + "\n" + result.stdout + result.stderr + diagnostic)
                if not success and result.returncode == 0:
                    raise RuntimeError("CLI unexpectedly accepted unsafe operation")
                return result

            cli("doctor")
            cli("inspect", "--out", "inventory.json")
            cli("plan", "--inventory", "inventory.json", "--out", "plan.json")
            unsafe_inventory = json.loads((path / "inventory.json").read_text())
            unsafe_inventory["database"]["foreign_keys"].append({"target_schema":"auth"})
            (path / "unsafe-inventory.json").write_text(json.dumps(unsafe_inventory))
            cli("plan", "--inventory", "unsafe-inventory.json", "--out", "blocked-plan.json", success=False)
            cli("apply", "--plan", "blocked-plan.json", "--phase", "database", "--run", "blocked-run", "--dry-run", success=False)
            plan_digest = json.loads((path / "plan.json").read_text())["digest"]
            cli("auth-export", "--plan", "plan.json", "--out", "enrollment-source.json")
            (path / "enrollment.json").write_text(json.dumps({"version":1,"plan_digest":plan_digest,"mappings":[{"source_id":ACTIVE,"target_subject":"enrolled-fixture-user","approved_by":"synthetic-test","proof_reference":"fixture-controlled-account"}]}))
            common = ["--plan","plan.json","--run","run","--ack-plan",plan_digest,"--writes-frozen"]
            cli("apply", *common, "--phase", "prepare")
            cli("apply", *common, "--phase", "database")
            cli("apply", *common, "--phase", "database", success=False)
            sql("target", "UPDATE neon_auth.\"user\" SET banned=true;")
            cli("apply", *common, "--phase", "auth", "--enrollment", "enrollment.json", success=False)
            sql("target", "UPDATE neon_auth.\"user\" SET banned=false;")
            cli("apply", *common, "--phase", "auth", "--enrollment", "enrollment.json")
            cli("apply", *common, "--phase", "auth", "--enrollment", "enrollment.json")
            cli("apply", *common, "--phase", "storage")
            cli("apply", *common, "--phase", "storage")
            cli("verify", "--plan", "plan.json", "--enrollment", "enrollment.json", "--out", "verification.json")
            evidence = json.loads((ROOT / "examples/evidence.json").read_text())
            evidence["plan_digest"] = plan_digest
            (path / "stale-evidence.json").write_text(json.dumps(evidence))
            cli("cutover-check", "--plan", "plan.json", "--enrollment", "enrollment.json", "--evidence", "stale-evidence.json", "--out", "not-ready.json", success=False)
            for key in evidence:
                if key not in ["plan_digest", "recorded_at_unix"]:
                    evidence[key] = "synthetic-fixture-attestation"
            evidence["recorded_at_unix"] = int(time.time())
            (path / "evidence.json").write_text(json.dumps(evidence))
            cli("cutover-check", "--plan", "plan.json", "--enrollment", "enrollment.json", "--evidence", "evidence.json", "--out", "readiness.json")
            assert json.loads((path / "readiness.json").read_text())["cutover_ready"] is True
            sql("target", "SELECT setval('parts_id_seq',1,true);")
            cli("verify", "--plan", "plan.json", "--enrollment", "enrollment.json", "--out", "bad-sequence.json", success=False)
            sql("target", "SELECT setval('parts_id_seq',100,true);")
            sql("target", "UPDATE parts SET price_minor=1 WHERE id=1;")
            cli("verify", "--plan", "plan.json", "--enrollment", "enrollment.json", "--out", "must-not-exist.json", success=False)
            assert not (path / "must-not-exist.json").exists()
            cli("report", "--run", "run")
            print("PASS: database restore, sequence state, Auth mapping, disabled identity, S3 multipart, Unicode, metadata, empty files, safe retries, and content-drift rejection.")
            print("This is a local protocol rehearsal. It does not establish Neon provider compatibility.")
        finally:
            for name in [pg, s3]:
                subprocess.run([DOCKER,"rm","-f",name], capture_output=True)


if __name__ == "__main__":
    main()
