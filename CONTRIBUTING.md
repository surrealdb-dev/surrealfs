# Contributing to SurrealFS

SurrealFS is currently a design repository. Contributions should make the product contract more
precise, falsifiable, and implementable.

## Before proposing implementation

Read `README.md`, `PLAN.md`, docs 00-08, and the ADRs. A change to commit, filesystem, branch,
provenance, durability, security, migration, or storage behavior must update the corresponding
design and its tests in the same change.

## Design rules

- Keep the domain model independent of SurrealDB types.
- Keep one production storage adapter until a documented decision trigger fires.
- Do not add a mutation path around `surrealfsd` and the semantic kernel.
- Do not infer causal history that was not captured.
- Do not call nondeterministic reruns deterministic replay.
- Make tenant/repository scope explicit on persistent entities and queries.
- Treat logical export as a compatibility contract.
- Add bounds to traversal, streaming, transaction, and allocation behavior.
- State what fails after a crash and what success acknowledges.

## Schema and query changes

`schema/001-core.surql` is a design draft, not a migration to run against user data. Production
work must split schema changes into monotonic numbered migrations. Each persistent change needs:

- forward migration and startup compatibility behavior;
- logical export/import representation;
- upgrade and interrupted-migration tests;
- indexes justified by a named query/workload;
- tenant/repository isolation review;
- behavior for historical records and old clients.

Queries must be parameterized, named, scoped, bounded, reviewed, and integration tested against the
exact pinned SurrealDB revision. Do not assemble SurrealQL using user-supplied strings.

## Testing expectations

Every mutating feature needs tests for success, conflict, request replay, authorization/policy,
engine error, and reopen. Transactional changes add a named fault point and invariant assertions.
Encoding/hash changes add cross-version golden vectors. Performance-sensitive changes include a
reproducible benchmark with durability and configuration stated.

## Architecture decisions

Create an ADR when a change alters a durable promise, trust boundary, engine/runtime topology,
public compatibility surface, or accepted high-impact risk. Do not rewrite an accepted ADR to hide
the prior decision; supersede it with a new record.

## Security

Never commit real customer/agent data, tokens, private prompts, model outputs, database directories,
or encryption keys. Fixtures must be generated or explicitly sanitized. Security-sensitive changes
to path walking, authorization, policy, imports, raw queries, migrations, recovery, or cryptography
require a named security reviewer.

## Documentation quality

- Use canonical terms from `docs/glossary.md`.
- Separate guarantees, current implementation, proposal, estimate, and hypothesis.
- Include evidence and failure/stop criteria for technology claims.
- Prefer a small precise example over a broad unsupported claim.
- Keep README links and the document map working.

## License

No SurrealFS project license has been selected yet. Do not contribute production code or assume
redistribution terms until repository maintainers publish a license and contributor policy. The
licenses of SurrealDB, SurrealKV, and every dependency must be reviewed independently.
