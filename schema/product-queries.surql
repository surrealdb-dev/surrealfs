-- SurrealFS representative product-query draft
--
-- These statements illustrate the intended graph access patterns. They are not a public raw API
-- and are not yet validated migrations. Production code keeps each query in a separately named,
-- parameterized operation, resolves record parameters through typed SDK values, injects
-- tenant/repository scope, checks indexes/query plans, and applies result/traversal limits.

-- Q1: Explain the direct captured author of one commit.
-- Bind: $tenant, $repository, $target_commit (typed record<commit>)
SELECT
    id,
    kind,
    committed_at,
    state_root,
    state_root_digest,
    <-caused[WHERE tenant = $tenant AND repository = $repository]<-(span, tool_call) AS direct_causes,
    ->produced[WHERE tenant = $tenant AND repository = $repository]->artifact AS artifacts,
    <-governed[WHERE tenant = $tenant AND repository = $repository]<-policy_decision AS policy_decisions
FROM ONLY $target_commit
WHERE tenant = $tenant AND repository = $repository;

-- Q2: Show an artifact's production and derivation lineage.
-- Bind: $tenant, $repository, $target_artifact
SELECT
    id,
    kind,
    content_hash,
    <-produced[WHERE tenant = $tenant AND repository = $repository]<-(commit, span, tool_call) AS producers,
    ->derived_from[WHERE tenant = $tenant AND repository = $repository]->artifact AS source_artifacts,
    <-derived_from[WHERE tenant = $tenant AND repository = $repository]<-artifact AS derived_artifacts,
    ->materializes[WHERE tenant = $tenant AND repository = $repository]->commit AS materialized_at
FROM ONLY $target_artifact
WHERE tenant = $tenant AND repository = $repository;

-- Q3: Resolve commits authored by tool calls in one run.
-- This two-step form is intentionally straightforward; inspect EXPLAIN before selecting the final
-- production query/index shape.
LET $tool_spans = SELECT VALUE id FROM (
    SELECT id, started_at FROM span
        WHERE tenant = $tenant
          AND repository = $repository
          AND run = $run
          AND kind = "TOOL_CALL"
        ORDER BY started_at ASC, id ASC
        LIMIT $span_limit
);

SELECT
    id,
    name,
    started_at,
    ->caused[WHERE tenant = $tenant AND repository = $repository]->commit AS commits
FROM $tool_spans
WHERE tenant = $tenant AND repository = $repository;

-- Q4: One correct first-parent log step.
-- Bind $cursor to the branch head for the first call, then to `next_cursor` for each subsequent
-- call until NONE or the product-level limit is reached. Generation is returned as metadata only;
-- filtering a repository by generation is not ancestry and can leak sibling-branch commits.
SELECT
    id,
    kind,
    generation,
    committed_at,
    message,
    state_root,
    state_root_digest,
    author_span,
    first_parent AS next_cursor
FROM ONLY $cursor
WHERE tenant = $tenant
  AND repository = $repository;

-- Q5: Compare evaluations for candidate runs.
SELECT
    out AS subject,
    in.name AS evaluation_name,
    in.evaluator AS evaluator,
    in.evaluator_version AS evaluator_version,
    in.score AS score,
    in.passed AS passed,
    in.created_at AS evaluated_at
FROM evaluated
WHERE tenant = $tenant
  AND repository = $repository
  AND out IN $candidate_runs
  AND in.name = $evaluation_name
ORDER BY score DESC
LIMIT $limit;

-- Q6: Find forks from one commit.
SELECT
    in AS fork,
    out AS fork_point
FROM forked_from
WHERE tenant = $tenant
  AND repository = $repository
  AND out = $commit
LIMIT $limit;

-- Q7: Policy decisions governing one target.
SELECT
    in.id AS decision,
    in.outcome AS outcome,
    in.rule_ids AS rule_ids,
    in.policy_bundle_hash AS policy_bundle_hash,
    in.reason AS reason,
    in.created_at AS created_at
FROM governed
WHERE tenant = $tenant
  AND repository = $repository
  AND out = $target
ORDER BY in.created_at ASC
LIMIT $limit;

-- Q8: Durable event catch-up after a client cursor.
SELECT * FROM span_event
WHERE tenant = $tenant
  AND repository = $repository
  AND domain_sequence > $after_sequence
  AND domain_sequence <= $read_boundary
ORDER BY domain_sequence ASC
LIMIT $limit;

-- Q9: Mutations touching one stable target.
SELECT
    commit,
    ordinal,
    kind,
    path_before,
    path_after,
    before_hash,
    after_hash
FROM commit_mutation
WHERE tenant = $tenant
  AND repository = $repository
  AND target_kind = $target_kind
  AND target_id = $target_id
ORDER BY commit, ordinal
LIMIT $limit;

-- Q10: Artifact chain from a tool call to evaluated outputs.
SELECT
    id,
    tool_name,
    ->produced[WHERE tenant = $tenant AND repository = $repository]->artifact AS direct_artifacts,
    ->produced->artifact->derived_from->artifact AS input_lineage,
    ->produced->artifact<-evaluated<-evaluation AS evaluations
FROM ONLY $tool_call
WHERE tenant = $tenant AND repository = $repository;

-- Q11: Integrity probe for edges that violate repository scope.
-- Run only as an administrator/verifier. Similar probes exist for every relation table.
SELECT id, in, out, repository
FROM caused
WHERE repository != in.repository OR repository != out.repository;

-- Q12: Branches whose duplicated generation does not match their head.
SELECT id, head, head_generation, head.generation AS actual_generation
FROM branch
WHERE head_generation != head.generation;

-- Q13: Candidate unreachable staged chunks for GC review.
-- This is discovery only. GC requires a conservative reachability algorithm and never deletes
-- directly from this query result.
SELECT id, digest, length, staged_by, created_at
FROM chunk
WHERE tenant = $tenant
  AND repository = $repository
  AND state = "STAGED"
  AND created_at < $staging_cutoff
ORDER BY created_at ASC
LIMIT $limit;

-- Q14: Migration confidence report.
SELECT capture_quality, count() AS records
FROM span
WHERE tenant = $tenant AND repository = $repository
GROUP BY capture_quality;
