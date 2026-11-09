import 'json.dart';
import 'memory_fact.dart';

/// `GET /archive/sessions/{id}/memory`: what generating would do for one
/// conversation, and the job running for it, if any.
class SessionMemoryStatus {
  const SessionMemoryStatus({
    required this.supported,
    required this.pendingTurns,
    required this.maxTurnsPerJob,
    required this.memoryCount,
    this.job,
    this.otherJobRunning = false,
  });

  /// The source's parser feeds extraction; some sources have none yet.
  final bool supported;

  /// An upper bound on the turns not yet analysed.
  final int pendingTurns;

  /// One click analyses at most this many turns; the next resumes.
  final int maxTurnsPerJob;
  final int memoryCount;
  final SessionMemoryJob? job;

  /// Another job (another session, or a small-sample run) holds the slot.
  final bool otherJobRunning;

  bool get running => job?.running ?? false;

  factory SessionMemoryStatus.fromJson(Map<String, dynamic> j) =>
      SessionMemoryStatus(
        supported: asBool(j['supported']),
        pendingTurns: asIntOrNull(j['pending_turns']) ?? 0,
        maxTurnsPerJob: asIntOrNull(j['max_turns_per_job']) ?? 30,
        memoryCount: asIntOrNull(j['memory_count']) ?? 0,
        job: j['job'] is Map
            ? SessionMemoryJob.fromJson((j['job'] as Map).cast())
            : null,
        otherJobRunning: asBool(j['other_job_running']),
      );
}

class SessionMemoryJob {
  const SessionMemoryJob({
    required this.id,
    required this.status,
    required this.total,
    required this.processed,
    required this.memoryCount,
    this.error,
  });
  final String id, status;
  final int total, processed, memoryCount;
  final String? error;
  bool get running => status == 'running';

  factory SessionMemoryJob.fromJson(Map<String, dynamic> j) => SessionMemoryJob(
    id: asString(j['id']),
    status: asString(j['status']),
    total: asIntOrNull(j['total_messages']) ?? 0,
    processed: asIntOrNull(j['processed_messages']) ?? 0,
    memoryCount: asIntOrNull(j['memory_count']) ?? 0,
    error: asStringOrNull(j['last_error']),
  );
}

/// What an id names (`GET /lookup/{id}`).
enum IdKind { session, memory, summary, episode }

class IdLookup {
  const IdLookup({
    required this.kind,
    required this.id,
    this.sessionId,
    this.title,
  });
  final IdKind kind;
  final String id;
  final String? sessionId, title;

  factory IdLookup.fromJson(Map<String, dynamic> j) => IdLookup(
    kind: IdKind.values.firstWhere(
      (k) => k.name == j['kind'],
      orElse: () => IdKind.episode,
    ),
    id: asString(j['id']),
    sessionId: asStringOrNull(j['session_id']),
    title: asStringOrNull(j['title']),
  );
}

List<MemoryFact> factsOf(Map<String, dynamic> j) => (j['facts'] as List? ?? [])
    .whereType<Map>()
    .map((m) => MemoryFact.fromJson(m.cast<String, dynamic>()))
    .toList();

/// A ULID, as pasted: 26 Crockford base-32 characters, any case.
final ulidPattern = RegExp(r'^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$');

/// The shared job slot also runs per-session jobs; their counts are QA turns,
/// not messages, and they name one session.
bool isSessionJob(Map<String, dynamic>? job) {
  final first = (job?['selections'] as List?)?.firstOrNull;
  return first is Map && first['mode'] == 'session';
}
