-- SurrealFS migration 0003: tool-call duration, for statistics.
--
-- Recorded at finish time rather than derived at query time, so aggregates do not depend on
-- arithmetic over a linked record's timestamps. A call that never finished has no duration,
-- which is what keeps an interrupted call out of the averages instead of counting as instant.

DEFINE FIELD duration_ms ON TABLE tool_call TYPE option<int>;
DEFINE INDEX tool_call_repo_name_started ON TABLE tool_call FIELDS repository, tool_name, started_at;
