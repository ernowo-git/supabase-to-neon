# Reviewed schema adaptations

An adaptation combines trusted operator-authored SQL, exact archive exclusions, and a schema expectation captured from a rehearsal. Use it when the source has Supabase Auth foreign keys, provider-specific policies, functions, or extensions that cannot restore unchanged.

The CLI does not infer replacement authorization rules or parse and rewrite function bodies. Review the SQL with the same care as a privileged database migration. It executes through `psql` using target migration credentials. Plans contain the complete scripts and authorization queries, so keep them private and keep secrets out of SQL.

## Prepare the expectation

1. Inspect the source and an empty destination. Save that inventory before preparing any target schema.
2. Save a plan even if it reports blockers, then export the source archive's schema entries.

```sh
just cli inspect --out inventory.json
just cli plan --inventory inventory.json --out blocked-plan.json
just cli archive-list --plan blocked-plan.json --out archive-list.txt
```

`plan` returns a nonzero exit code when blockers exist, but saves the plan for review. `archive-list` is read-only against services and dumps schema definitions locally. It accepts a valid blocked plan for this purpose.

3. Build the intended destination schema in a separate disposable rehearsal target, using reviewed SQL. Include the intended grants and function owners. Capture it with `inspect`; its `target_database` becomes the schema expectation. This is the expected structure, not expected table contents.
4. Create a template using the original empty-target inventory and the rehearsal inventory.

```sh
just cli adaptation-template --inventory inventory.json --expected-inventory expected-inventory.json --out adaptation.json
```

The empty-target inventory must describe the actual migration destination. The expected inventory can come from another disposable target. Do not point the final run at a target containing the rehearsal tables.

## Complete the artifact

Set `reviewed_by` and `review_reference` to the approver and review record. The source snapshot digest is filled by the CLI. Complete these fields:

| Field | Purpose |
| --- | --- |
| `before_restore_sql` | Prepare reviewed roles, extensions, and helper functions needed during restore |
| `exclude_toc` | Exact normalized schema-entry descriptions copied from `archive-list.txt` |
| `after_restore_sql` | Recreate translated foreign keys, policies, grants, and other excluded schema objects |
| `expected_database` | Full expected target schema inventory from a rehearsal |
| `authorization_checks` | Read-only checks under restricted runtime roles |

For an archive line such as `123; 3256 456 POLICY public notes owner_only postgres`, the exclusion is `POLICY public notes owner_only postgres`. Do not include the archive ID or catalog OIDs. Every exclusion must match exactly one entry. Table definitions, table data, and sequence-state entries cannot be excluded. Unknown or ambiguous entries stop the run before preparation SQL executes.

An authorization check has this shape:

```json
{
  "name": "missing identity sees no notes",
  "role": "app_reader",
  "actor_id": null,
  "sql": "SELECT to_json(count(*)) FROM public.notes;",
  "expected": 0
}
```

The CLI starts a read-only transaction, selects the supplied role, binds `app.actor_id` transaction-locally, runs the query, and rolls back. Queries must return one JSON value. It rejects roles that own selected application objects, inherit that ownership, are superusers, or have RLS bypass. Supply positive and negative cases, including disabled, missing, and unrelated identities. Query authors remain responsible for checking the application's actual permission model. This mechanism does not validate application sessions or replace write-path tests.

The server must derive `app.actor_id` from a verified session. A client with arbitrary SQL access could set this variable itself. Keep runtime SQL credentials on the trusted server.

Definer functions need an explicit expected owner and fixed `search_path`. Verification checks that their owners cannot log in and have neither superuser nor RLS bypass. Function execution grants are compared against the expected inventory. Keep definer owners and function grants narrow, and include checks of their effects.

## Plan and run

```sh
just cli plan --inventory inventory.json --adaptation adaptation.json --out plan.json
```

The plan digest covers the complete adaptation. Source drift, edited scripts, changed exclusions, and changed expectations invalidate the plan. The adaptation must preserve the application table set, column layout and types, generated semantics, indexes, and named constraints. Ordinary defaults and foreign-key targets may change under review. Adapted application tables must have RLS enabled.

Enroll and map accounts before database transfer when a translated foreign key references `migration_identity.principal`. Run the phases in this order: prepare, Auth, database, storage, verify. Every mutation still requires the reviewed plan digest and a write-freeze acknowledgment.

Database transfer runs preparation SQL, filtered archive restore, and completion SQL in that order. Each stage uses its own PostgreSQL transaction; the combined process is not atomic. Scripts must not contain explicit transaction-control statements if their stage must remain atomic. Any failure requires inspection and a fresh empty target before retrying. Roles and other cluster-level objects may survive a failed run and need deliberate cleanup or reviewed idempotent preparation.

After transfer, verification compares the actual schema, extension versions, table grants, routine grants, and function ownership against the saved expectation. It also runs the authorization checks, compares complete table contents against the frozen source, checks sequence state, and verifies principal mappings and object storage.

A managed default embedded inside a table definition still has to resolve during restore. This version cannot replace a table's entire DDL or change its data columns. Resolve that dependency explicitly in reviewed preparation SQL, or treat it as an unsupported migration. Do not create or remove target provider-owned objects to mimic Supabase without first verifying the provider contract.

The expanded inventory requires fresh inventories and plans when upgrading from alpha.1. Existing alpha.1 artifacts are not accepted as current migration evidence.

The restore selection uses PostgreSQL's documented archive list support. Review source-defined executable code as well as the replacement scripts. [PostgreSQL pg_restore](https://www.postgresql.org/docs/17/app-pgrestore.html)
