# Risk Register

## Rating method

- Probability: `Low`, `Medium`, `High` over the next 18 months.
- Impact: `Moderate`, `High`, `Critical` if realized.
- Owners are roles to assign when implementation starts.
- A mitigation reduces likelihood/impact; a contingency describes what happens after the risk
  materializes.

This register is reviewed at every phase gate and before every production release. “Accepted” risks
still need monitoring and an owner.

## Technical and data risks

| ID | Risk | P | Impact | Leading indicators | Mitigation | Contingency | Owner |
|---|---|---:|---:|---|---|---|---|
| T1 | SurrealKV beta defects cause corruption or acknowledged-data loss | M | Critical | reopen mismatch, checksum errors, upstream critical fixes such as 0.21.3 compaction-fsync/memtable changes, flaky crash seeds | pinned revision; deterministic fault/compaction matrix; checksums/state roots; logical exports; support relationship | stop writes, quarantine, restore verified export/copy; replace storage adapter if systemic | Database lead |
| T2 | Nightly/private SurrealDB API or behavior changes rapidly | H | High | frequent compile/schema/query breakage; >10% capacity on upgrades | depend only on supported SDK; pin exact revision; adapter isolation; scheduled upgrades | hold known-good version; upstream/contract support; replace adapter | Database lead |
| T3 | SurrealDB transaction semantics do not cleanly support expected-head + root/graph commit | L/M | Critical | conflicts create partial records, non-repeatable concurrency tests | Phase 0 parity transaction proof; single semantic writer; receipt/invariant suite | redesign transaction encoding; reject engine if atomicity cannot be proven | Storage lead |
| T4 | SurrealQL/query layer is too slow for root-based metadata hot paths | M | High | p99 stat/list/open exceeds SLO; high rows scanned/CPU | persistent-node layout benchmarks; composite indexes; reviewed queries; caching by immutable root; optional disposable head projection | narrow POSIX scope; measured root-keyed projection; replace engine if structural | Filesystem lead |
| T5 | Large semantic commits exceed transaction limits or cause stalls | H | High | commit latency superlinear with mutations; memory spikes; compaction stalls | workspace limits; chunk staging; bounded mutation batches with one visibility transaction; benchmark 1k/10k+ | split semantic operation using hidden staging/final publish or reduce supported operation size | Storage lead |
| T6 | History, graph, and secondary indexes cause unsustainable disk growth | H | High | disk/logical ratio rising; compaction debt; backups exceed RPO/RTO | explicit retention; chunk dedup; compact events; measured indexes; reachability GC; quotas | archive cold history to logical packs; prune opt-in data; pricing/limits | Reliability owner |
| T7 | Value-log/compaction behavior creates tail latency | M | High | p99.9 spikes correlate with compaction/VLog GC | workload-specific thresholds; maintenance budgets; foreground latency benchmarks; observability | reschedule/throttle maintenance, tune layout, isolate artifact packs, change adapter | Database lead |
| T8 | Application history duplicates SurrealKV temporal history | M | Moderate/High | versioned storage doubles growth without product use | keep versioning off by default; finite diagnostic retention; test independence | disable/version-prune after supported export and verification | Database lead |
| T9 | Schema evolution breaks historical queries or upgrades | M | Critical | migrations cannot resume; old exports fail; graph counts change | additive migrations; schema manifest; cloned upgrade; golden historical fixtures; stable domain views | restore old copy, export with old binary, fix forward in new target | Migration owner |
| T10 | Content/state hash design has ambiguity or becomes incompatible | L/M | Critical | different states share canonical bytes/root due to encoding bug; nondeterminism across platforms | domain-separated, versioned canonical encoding; golden cross-language vectors; formal review | introduce new hash version and dual-root migration; quarantine affected histories | Storage lead |
| T11 | Garbage collection deletes reachable data | L | Critical | verification finds missing chunks after GC; reachability count disagreement | conservative mark/sweep from explicit roots; two-phase tombstone; dry-run; model/property tests | stop GC; restore packs/export; rebuild reachability; ship regression | Reliability owner |
| T12 | Garbage collection never reclaims enough | H | Moderate/High | unreachable staging/chunks grow; full scans too slow | reference accounting plus periodic mark verification; incremental epochs; quotas | offline compaction/export-import rebuild; adjust retention | Reliability owner |
| T13 | Branch ancestry becomes too deep for logs/merge | M | High | first-parent/LCA latency scales linearly with commit depth | direct parent traversal; verified ancestor skip indexes/checkpoints; immutable roots avoid ancestry lookup for state | bounded cursors and measured skip index; never substitute generation range for ancestry | Storage lead |
| T14 | Filesystem semantic bugs corrupt user expectations | H | Critical | fstests failures, editor/build incompatibility, stale handles | explicitly narrow POSIX contract; reference model; mount corpus; canary workloads | disable affected operation/mount mode; preserve direct API; recovery tooling | Filesystem lead |
| T15 | Chunk dedup/hash creates privacy equality oracle | M | High in multi-tenant | cross-tenant dedup timing/storage differences | tenant-scoped keyed IDs or physical separation; constant response semantics | disable shared dedup; migrate chunks to tenant scope | Security owner |
| T16 | Logical export is incomplete or cannot restore at scale | M | Critical | restore count/root mismatch; export exceeds window/memory | build in Phase 1; streaming; terminal index/checksums; recurring drills | preserve physical copies and old binary; stop upgrades until fixed | Reliability owner |
| T17 | Single-daemon design becomes throughput/availability bottleneck | M | High | CPU saturation, write queue growth, maintenance downtime unacceptable | concurrency inside daemon; workload budgets; fast restart; read scaling only when safe | partition by repository/tenant; active-passive orchestration; revisit distributed design | Architecture owner |
| T18 | A second process opens the database and corrupts ownership assumptions | L/M | Critical | lock failures, unexplained engine open errors | exclusive OS lock; private permissions; no direct SDK path; startup identity checks | quarantine and verify; operator repair from backup | Reliability owner |
| T19 | Subscription semantics lose or reorder user-visible events | M | High | cursor gaps, duplicates treated as new, live/catch-up race | durable domain sequence + catch-up; at-least-once contract; dedup IDs | force resync from query/export; rebuild consumer projection | Protocol lead |
| T20 | External tool effects cannot be replayed safely | H | High | duplicate emails/API mutations; UNKNOWN outcomes | provider idempotency keys; intention/outbox; explicit effect records; no blind replay | require human reconciliation/approval; mark irreproducible | Integrations owner |
| T21 | Public embedded SDK lifecycle cannot provide observable clean shutdown and lock release | M | High | reopen races, store remains locked, shutdown flush/close error only appears in logs | public-API drop/reopen contract tests; daemon drain; crash correctness independent of close; request supported awaited shutdown | restart in new process after verified release; hold known-good SDK; upstream public lifecycle capability | Database/reliability |
| T22 | Immutable roots, mutation evidence, and optional head projections diverge | M | Critical | rebuilt projection differs; root digest mismatch; historical read disagrees with head | one canonical root; path-copy reference model; cross-store golden vectors; projection rebuild/compare; quarantine on mismatch | disable/drop projection, serve root reads, quarantine corrupt commit, restore verified export | Storage lead |

## Security and privacy risks

| ID | Risk | P | Impact | Leading indicators | Mitigation | Contingency | Owner |
|---|---|---:|---:|---|---|---|---|
| S1 | Raw SurrealQL writes bypass provenance/invariants | M | Critical | records without commits/authors; operator scripts mutate tables | read-only scoped interface; offline gated recovery; DB inaccessible to clients; invariant scan | quarantine, identify last trusted root, restore/rebuild, revoke access | Security owner |
| S2 | Cross-tenant record or graph traversal leaks data | M | Critical | missing scope predicates; shared IDs; negative test failure | typed scoped IDs; tenant/repo on records; relation assertions; separate DBs initially; adversarial suite | disable shared tenancy, notify/contain, rotate, forensic audit | Security owner |
| S3 | Secrets leak through traces, previews, logs, exports, or embeddings | H | Critical | scanners find credentials; high capture volume; unclear classifications | capture minimization; tool schemas; pre-persistence redaction; field authorization; encrypted artifacts | revoke secrets, restrict/export purge per policy, notify, improve detector and tests | Security owner |
| S4 | Database-directory theft exposes plaintext | M | Critical | unencrypted hosts/backups; overly broad permissions | encrypted volume; daemon-only permissions; envelope encryption for sensitive artifacts | rotate credentials/keys, assess exposure, restore into secure deployment | Operations/security |
| S5 | Malicious import/archive escapes repository or exhausts service | M | High | crashes/OOM during import; odd paths or compression ratios | streaming hostile-input parser; quotas; staging invisibility; fuzzing | stop job, discard isolated staging through approved procedure, patch parser | Migration owner |
| S6 | Graph query amplification causes denial of service | H | High | explosive edges/rows, long read locks, memory spikes | depth/node/time/byte limits; cursors; indexed allowlisted views; quotas | cancel queries, disable raw graph endpoint, lower limits | Protocol lead |
| S7 | Administrator/recovery mode is abused | L/M | Critical | unexplained maintenance sessions or audit gaps | local explicit enable; MFA/approval; backup prerequisite; signed/chained audit; no remote agents | revoke admin, quarantine, compare external checkpoints, restore | Security owner |
| S8 | Audit/state roots give false sense of tamper proofing | M | High | no external anchor; whole-store rollback undetectable | honest threat model; signed external checkpoints for assurance tiers | compare external anchor/backups; disclose uncertainty | Security owner |
| S9 | Privacy “data flywheel” violates consent or regulation | M | Critical | secondary use unclear; deletion impossible; sensitive derived models | opt-in purpose limitation; tenant isolation; minimization; export/delete; governance review | suspend aggregation/training, delete where required, notify and remediate | Privacy/product |
| S10 | Dependency or migration supply chain is compromised | L/M | Critical | unsigned release, unexpected dependency change, SBOM alerts | pins, signed releases/migrations, SBOM/attestation, reviewed upgrade diff | stop rollout, revoke signing material, restore known-good binary/data | Release owner |
| S11 | Tool forges attribution or bypasses the workspace boundary | H | Critical | commits with self-declared span only; writes from wrong/detached process; host/lower/database path access | daemon-issued capability; mount namespace+cgroup; read-only lower; DB inaccessible; missing-context reject; descendant/quiescence tests | disable captured claim/integration, abort workspace, quarantine affected evidence, fix launcher/sandbox | Security/runtime |

## Product and moat risks

| ID | Risk | P | Impact | Leading indicators | Mitigation | Contingency | Owner |
|---|---|---:|---:|---|---|---|---|
| P1 | Users see SurrealFS as a database/filesystem swap, not a high-value workflow | H | Critical | interest focuses on benchmarks; low explain/fork usage | lead with failure recovery/fork-compare/provenance outcomes; design partners; opinionated UX | narrow to strongest workflow or stop general platform | Product owner |
| P2 | Database choice is mistaken for moat and is easily copied | H | High | roadmap dominated by engine; no unique integrations/recovery outcomes | mandatory parity spike; measure causal completeness, repeated recovery, trust, integration coverage | treat storage as replaceable; redirect investment to product loop | Product/architecture |
| P3 | Capture is incomplete, so users do not trust explanations | H | Critical | unattributed commits, missing tool inputs, manual correlation persists | capability-bound workspace/process scope; framework adapters; completeness metrics; fail closed | limit claims to verified scope; disable integration; block commits lacking required attribution | Domain/security |
| P4 | Fork/diff/merge semantics are too complex for common agents | M | High | conflicts overwhelm users; manual copy remains easier | focus on checkpoint/fork and compare before broad merge; opinionated strategies | omit general merge; export chosen branch/artifact | Product/filesystem |
| P5 | Git, tracing vendors, sandboxes, or databases add similar features | H | High | competitors bundle sufficient provenance/forks cheaply | enforced file+KV+span boundary; integrations; recovery outcome; OTel/provenance interoperability | integrate with winners or specialize; stop separate product if compound value is not material | Product owner |
| P6 | Integration surface fragments across agent frameworks | H | High | adapters break often; causal coverage differs | framework-neutral protocol; small adapter kit; prioritize top frameworks; conformance metrics | support fewer integrations deeply; partner ecosystem | Integrations owner |
| P7 | Claimed replay is nondeterministic and damages trust | H | High | reruns differ due to model/time/network/randomness | distinguish state reconstruction from behavioral replay; capture manifests/effects; confidence labels | rename/narrow feature; offer fork-and-rerun with explicit uncertainty | Product/domain |
| P8 | Storage/trace cost exceeds customer value | M/H | Critical | low willingness to pay, high retention and egress | retention tiers; artifact policies; dedup; outcome pricing; cost telemetry | cold archive/prune, narrower capture, revise packaging | Product/finance |
| P9 | High migration/adoption friction prevents trials | H | High | long setup, mount incompatibility, source fear | read-only shadow import; SDK/direct API; verified loss report; reversible pilot | offer capture-only mode or sidecar integration | Developer experience |
| P10 | A speculative execution-data flywheel distracts from the initial product | H | High | roadmap depends on central data; weak consent/volume/labels; privacy work precedes recovery value | remove corpus from initial thesis/gates; local-first minimization; optional use-specific consent only | moat rests on workflow/integrations/trust; do not centralize data | Product/privacy |
| P11 | Building broad POSIX support consumes the company before value proof | H | Critical | roadmap stalls in compatibility; no partner workflow shipped | vertical slice; explicitly narrow subset; phase gates before broad mount | prioritize direct workspace API; defer general filesystem | Leadership |
| P12 | Users require distributed collaboration before single-node product matures | M | High | shared remote repositories are table stakes in discovery | make export/protocol/IDs distribution-ready; separate repositories; avoid false multiwriter promises | hosted daemon per repo/tenant; revisit distributed engine only with demand | Architecture/product |

## Legal and dependency risks

| ID | Risk | P | Impact | Leading indicators | Mitigation | Contingency | Owner |
|---|---|---:|---:|---|---|---|---|
| L1 | SurrealDB BSL terms conflict with distribution or managed offering | M | Critical | counsel flags service similarity/redistribution; license changes | Phase 0 legal opinion; exact version/feature/deployment review; commercial discussion | obtain commercial agreement, alter deployment, replace adapter | Legal/leadership |
| L2 | SurrealKV/SurrealDB support maturity is insufficient for SLA | M | High | beta persists; slow critical fixes; no support path | support agreement/upstream relationship; pinned fork capability; extensive validation | own patch branch temporarily; change engine before broad production | Leadership/database |
| L3 | Third-party FUSE/platform licenses or signing requirements delay shipping | M | High | kernel extension/notarization issues; enterprise policy blocks mounts | choose user-space supported adapters; legal/platform review early; direct API fallback | ship without mount on affected platform | Platform owner |
| L4 | Captured agent data creates regulatory discovery/retention obligations | H | High/Critical | customers request residency, legal hold, deletion, audit certifications | data inventory/classification; tenant controls; configurable retention/export/delete; DPA review | restrict markets/data classes; deploy customer-controlled/local only | Legal/security |

## Operational and organizational risks

| ID | Risk | P | Impact | Leading indicators | Mitigation | Contingency | Owner |
|---|---|---:|---:|---|---|---|---|
| O1 | Team lacks filesystem/database reliability expertise | M | Critical | recurring semantic bugs; slow incident resolution | explicit senior ownership; external review; narrow scope; fault infrastructure | pause production, hire/partner, reduce supported contract | Leadership |
| O2 | Documentation and implementation diverge | H | High | tests contradict guarantees; query/schema drift | executable invariants; docs in review checklist; ADRs; generated protocol refs | downgrade claim, fix docs/tests before release | Engineering lead |
| O3 | Benchmark results are misleading or irreproducible | M | High | laptop-only numbers, durability mismatch, missing raw output | standard harness/config manifests; comparative parity; raw results and CI canaries | retract result, rerun independently | Reliability owner |
| O4 | Backup exists but restore is too slow or untested | M | Critical | no recent drill; RTO grows with history | automated restore drills; scale tests; logical+physical paths; RTO telemetry | increase capacity, restore physical copy, narrow retention | Operations |
| O5 | Upgrade leaves repositories stranded between schemas | M | Critical | partial migration marker; old binary cannot open; new verifier fails | cloned upgrade, resumable phases, publish only after verify, logical export | serve old untouched copy; fix-forward in clone | Migration owner |
| O6 | Disk exhaustion cascades into corruption/unavailability | H | High | staging/compaction spikes, low-space alerts ignored | reservations, quotas, headroom, bounded staging, disk-full tests | reject writes, free only proven disposable staging under runbook, expand storage | Operations |
| O7 | Observability itself leaks data or causes cardinality explosion | H | High | paths/IDs in labels; telemetry cost spike | allowlisted low-cardinality labels; redaction; sampled structured logs | disable affected telemetry, purge/rotate access, ship fix | Reliability/security |
| O8 | Scope expands to RocksDB/SurrealKV-only/multiple production backends prematurely | M/H | High | adapter work exceeds product work; conformance matrix grows | two bounded Phase 0 spikes, then one production backend; domain trait only | stop extra backend work; preserve experiments out of release path | Architecture owner |

## Highest-priority risks before writing production code

1. `P1/P11`: prove a valuable workflow before paying for broad filesystem compatibility.
2. `T1/T3/T21/T22`: prove crash-safe root commit, conditional head movement, root/projection
   consistency, and public lifecycle on both Phase 0 candidates.
3. `L1`: settle license fit before architecture becomes expensive to change.
4. `T4/T5/T7`: measure hot-path and transaction/compaction behavior with real workloads.
5. `P3/S11`: prove capability/process-scoped capture; the causal graph is useless if state bypasses
   it or a caller can forge the author.
6. `S2/S3`: establish isolation and capture minimization before real customer data.
7. `T16/O4`: make logical recovery real from the first durable prototype.
8. `P1/P10`: validate repeated recovery value; do not substitute engine novelty or a speculative data
   flywheel for adoption evidence.

## Risk acceptance template

Any risk accepted for a release records:

```text
risk ID and exact scenario
release/deployment scope
evidence considered
remaining probability and impact
customer-visible limitation
monitor and alert
named owner
expiry/review date
contingency trigger and action
approver
```

An undocumented limitation is not risk acceptance; it is accidental exposure.
