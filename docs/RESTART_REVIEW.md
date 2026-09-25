# Runner and agent restart review — v1.29.35

The reviewed changes persist the ACP process identity, outstanding turn,
attachments and workflow bearer hash, then adopt surviving runner processes
at server startup. Active Job capsules stay available until the Job finishes
or someone stops them. This keeps their conversation, and also keeps their
login reservation and runner capacity: later steps may need additional logins
or a person to close an earlier capsule.

## Corrections made during review

- Register an adopted turn before reading buffered output, so an immediate
  reply cannot disappear and leave the turn permanently busy.
- Preserve unresolved permission requests and redisplay them after restart;
  a failed permission response leaves the question answerable.
- Ignore late/duplicate terminal replies when the request channel already has
  a response, including cancellation of an adopted turn.
- Include finished processes with queued output in the runner's hello. Their
  final bytes and exit drain before the broker declares the stream gone.
- Serialize replacement of an enabled agent within a capsule, including the
  interval before the new process is registered. Waiting starts honor context
  cancellation; other capsules can start independently.
- Never release an offline capsule's login merely because a day elapsed.
  Deleting a Job also requires a confirmed stop; otherwise its pending stop
  and login reservation remain until the runner reconnects.
- Retire capsules immediately when a Job finishes, rather than waiting for
  the periodic sweep.
- Do not repair a standing workflow decision while a persisted agent turn is
  still running: that would expose approval of a workspace still being edited.

Coverage is in `internal/server/restart_test.go`,
`internal/worker/broker_restart_test.go`,
`internal/worker/worker_agents_test.go`, `internal/store/restart_test.go`,
`internal/store/workflow_chat_test.go` and the existing app lifecycle test.
The full Go suite and the server/worker/store suites with `-race` passed.

## Operational limits

Update runners as well as the server to get stream reporting and serialized
agent replacement. The first upgrade from an older server cannot adopt an
agent identity that the older server never saved.

This is process adoption, not a durable, exactly-once message protocol. The
runner queue is memory-only; successful WebSocket writes are removed without
an application-level durable acknowledgment. A hard crash can lose an
in-flight prompt, response or output chunk, and unsent queued chat messages
and old conversation display history are not persisted. A resumed turn with
a lost reply may need cancellation or Retry. A runner restart does not
preserve its in-memory streams. Persisting agent metadata can also fail if
the state backend fails; those errors are logged. These limits were not
validated by actual Docker host crashes or a HopOS power-loss test.

The backup/replication investigation and its separate deployment requirements
are recorded in [the replication review](../replica/PRODUCTION_REVIEW.md).
