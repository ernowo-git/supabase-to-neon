# supabase-to-neon

A Rust CLI for inspecting and rehearsing a Supabase exit to Neon Postgres, Neon Auth, and Neon Object Storage.

This is an early alpha. It transfers compatible application schemas, records verified identity mappings after account enrollment, and streams S3 objects with independent SHA-256 verification. It does not import Supabase passwords or sessions. Current Neon documentation requires a separate account transition. [Neon Auth migration](https://neon.com/docs/auth/migrate/from-supabase)

Read [compatibility](docs/compatibility.md) before using real data. RLS policies, managed-schema references, cross-schema foreign keys, and non-default extensions require a [reviewed adaptation](docs/adaptations.md). Partitions remain unsupported. No production compatibility or zero-downtime guarantee is claimed.

## Build and check

Install Rust through rustup and install `just`. The pinned toolchain and Cargo.lock define the build.

```sh
just build
just check
just cli --help
```

Linux and macOS are the intended platforms. Windows is unsupported because protected run directories depend on Unix permissions. PostgreSQL 17 or newer client utilities are required for system certificate support. Client utility and server major versions must match in this alpha.

## Inspect and plan

```sh
just cli init
```

Edit `migration.toml` with explicit application schemas, project identifiers, bucket mappings, endpoints, and credential environment variable names. Supply connection strings and storage credentials through your shell or secret manager. The CLI does not load `.env` files. It does not provision projects, Auth, or bucket visibility.

Use direct PostgreSQL connections. Remote database URLs require `sslmode=verify-full`; `sslrootcert=system` is the default. A custom CA file may be supplied through the URL's `sslrootcert` option. HTTP and disabled database TLS are allowed only for loopback test services. Source credentials need complete read access, including bypassing source RLS for export; target migration credentials need DDL privileges. Runtime application credentials must remain separate and restricted.

```sh
just cli doctor
just cli inspect --out inventory.json
just cli plan --inventory inventory.json --out plan.json
just cli auth-export --plan plan.json --out enrollment-source.json
```

Inventory and plans may contain personal data and SQL function bodies. They are written with private permissions and never overwritten. Protect and dispose of these artifacts as database exports. A plan contains a digest of its inventory, nonsecret configuration, endpoint identities, and tool versions. This detects accidental edits and drift; it is not a cryptographic signature from a trusted reviewer.

The source SQL inspection reports known dependencies. Dynamic SQL, external services, jobs, and application behavior still require human review. Project IDs are operator labels; endpoint fingerprints, rather than provider account APIs, bind the inspected connections. Upgrades from alpha.1 require fresh inventories and plans.

For a source with managed dependencies, save the blocked plan, export the archive list, and bind a reviewed replacement to a destination schema captured during rehearsal:

```sh
just cli archive-list --plan plan.json --out archive-list.txt
just cli adaptation-template --inventory inventory.json --expected-inventory expected-inventory.json --out adaptation.json
just cli plan --inventory inventory.json --adaptation adaptation.json --out reviewed-plan.json
```

Complete the generated template before planning. It starts with empty review fields and checks and cannot execute as generated. The [adaptation guide](docs/adaptations.md) explains the expected-schema rehearsal, archive exclusions, SQL stages, and runtime authorization checks.

## Rehearse a transfer

Enroll users through supported Neon Auth flows. Create a private enrollment JSON file using the shape in [the example](examples/enrollment.json). Every active source user needs an approved mapping to an existing destination Auth account. Disabled source users remain unmapped. An approver and evidence reference attest verified enrollment; email equality never creates a mapping automatically.

The Auth phase creates application-owned `migration_identity.principal` and `migration_identity.provider_identity` tables. They preserve source UUIDs and deny public access through RLS and grants. Integrating these tables into application authorization and foreign keys is a separate reviewed application migration. The CLI does not write to managed Auth tables, send invitations, or verify proof-of-control documents itself.

Freeze database, Auth, storage, and background writes. Review the plan and pass its digest explicitly:

```sh
just cli apply --plan plan.json --phase prepare --run .runs/rehearsal --dry-run
just cli apply --plan plan.json --phase prepare --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli apply --plan plan.json --phase database --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli apply --plan plan.json --phase auth --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen --enrollment enrollment.json
just cli apply --plan plan.json --phase storage --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli verify --plan plan.json --enrollment enrollment.json --out verification.json
just cli report --run .runs/rehearsal
```

Database restore uses one PostgreSQL transaction and does not overwrite an existing application schema. An adaptation's preparation SQL, archive restore, and completion SQL are separate transactions. When translated foreign keys need application principals, run Auth before the database phase. A failed database run needs a new run directory and a verified empty target, with deliberate cleanup of any preparation objects. Auth and storage retries inspect remote results before proceeding. Storage refuses differing existing objects, never deletes keys, and re-reads bytes and metadata on resume. Do not allow concurrent writers to the destination.

The run journal records completed operations. An interrupted step is incomplete until checked again. SIGINT stops the CLI and kills its PostgreSQL child process. Interrupted multipart uploads may leave unfinished upload parts that need provider-side cleanup.

## Verify and cut over

Verification compares table row digests, schema definitions, sequence state, principal mappings, object key sets, metadata, and content digests. It reads complete table and object contents. Budget time and egress accordingly.

Use [cutover evidence](examples/evidence.json) to record application tests, restricted runtime RLS checks, storage access tests, the write freeze, and recovery rehearsal. Replace every example value with a reference to actual evidence. Set `recorded_at_unix` to the current Unix time. Evidence must be recorded within the previous hour.

```sh
just cli cutover-check --plan plan.json --enrollment enrollment.json --evidence evidence.json --out readiness.json
```

This command reruns data checks and records operator attestations. It does not validate external documents or switch traffic. The provider compatibility gate and application release review remain necessary. See [cutover](docs/cutover.md) and [recovery](docs/recovery.md).

## Preview stored URL changes

Copy and edit [the URL preview specification](examples/url-preview.json), binding it to the current plan digest and explicitly naming each table, text column, and old and new URL prefix.

```sh
just cli url-preview --plan plan.json --spec url-preview-spec.json --limit 100 --out url-proposals.json
```

The preview reads the verified destination schema and returns matching primary keys, current URLs, and proposed URLs. It records the total number of matches and whether the output is truncated. Each column is limited to 100 proposals by default, with a maximum of 10000. It counts all matches, so large tables can take time to scan.

Prefixes must end in a slash and cannot contain credentials or query strings. Text and varchar columns are supported; JSON, arrays, domains, generated columns, and tables without a primary key require separate review. Encoded path suffixes are preserved. Rows containing query strings or fragments are marked for manual review and receive no proposed URL. The command never updates rows or copies access policies. Recreate signed links through the target application. Applying URL changes later changes table content and requires separate reconciliation; it is not part of the original byte-for-byte database verification.

## Synthetic integration rehearsal

Install `tests/requirements.txt` in a Python virtual environment and allow Docker access, then run:

```sh
just rehearse
```

The script creates isolated PostgreSQL and Adobe S3Mock containers, exercises the CLI with synthetic records and objects, and removes those containers afterward. It never reuses existing databases or storage. PostgreSQL tools run in the disposable container. Default images are pinned by digest. Alternate images must expose the same PostgreSQL and S3Mock interfaces.

S3Mock and fixture Auth tables test protocol behavior, not Neon compatibility. S3Mock does not establish provider credential or access-policy behavior. Real Neon Auth and Object Storage need a disposable provider rehearsal before any production claim.

## Development status

Implemented: config checks, inspection, deterministic plans, reviewed schema and RLS adaptations, extension preparation through reviewed SQL, protected artifacts, run locking, journal records, database transfer, enrollment manifests and principal mappings, S3 transfer and content verification, technical readiness checks, and local failure tests.

Still required for the broader migration plan: automated provider-supported enrollment where available, a real Neon compatibility rehearsal, application session and write-policy proofs, broader failure injection, and measured operational limits. See [implementation status](docs/implementation-status.md).

MIT licensed. See [security](SECURITY.md) and [contributing](CONTRIBUTING.md).

Commands return 0 on success, 1 on a failed check or operation, 2 on invalid arguments, and 130 on interruption. JSON diagnostics go to stderr. No service mutation occurs in dry-run mode.
