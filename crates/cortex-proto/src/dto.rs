//! HTTP data transfer objects.
//!
//! This is the **one contract** shared by all three front ends: the CLI and the
//! Flutter desktop / web clients. Changing anything here means changing three
//! clients; tread carefully.

use serde::{Deserialize, Serialize};

// ─────────────────────────── /health ───────────────────────────

/// Takes **only the two protocol numbers** out of a peer's `/health`.
///
/// # Why not just deserialize [`Health`]
///
/// Two reasons, the second one being the hard one:
///
/// 1. Half the fields of `Health` are `&'static str`, which cannot be
///    deserialized into.
/// 2. More importantly, the **coupling surface**: the protocol check cares
///    about two numbers, whereas `Health` also carries database status, object
///    storage backend and vectorization health. Parsing with the whole `Health`
///    lets any change to those fields break the protocol check itself — and the
///    way a broken protocol check presents is precisely "nobody blocks an
///    incompatible peer".
///
/// # The default is 1, not 0
///
/// **This is the easiest thing in this type to get wrong.** Any cortexd from
/// before this check shipped (including the one running in production right
/// now) does not report these two fields at all. Default them to 0 and, on the
/// day the check takes effect, every desktop client refuses to start — when in
/// fact they would work perfectly well.
///
/// The correct meaning of an absent field is "this is a daemon predating this
/// check", i.e. protocol v1 (the version at the time the check was introduced),
/// not "protocol version zero".
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PeerProtocol {
    #[serde(default = "protocol_before_this_check")]
    pub protocol: u32,
    #[serde(default = "protocol_before_this_check")]
    pub min_peer_protocol: u32,
}

const fn protocol_before_this_check() -> u32 {
    1
}

/// The `/health` response — **shared by cortexd and the local agent**.
///
/// # Why there used to be three copies
///
/// cortexd had a strongly typed `Health`, the local agent hand-assembled a
/// `serde_json::json!`, and the CLI guessed at a third one of its own (with
/// `database` still required). The result: with `cortex --server` pointed at
/// the local agent, the `health` subcommand simply reported a decode failure —
/// observed in practice.
///
/// The root cause of the three copies was that this type had only `Serialize`:
/// clients could not read it back, so they wrote their own. It is now
/// bidirectional, and **every field whose shape differs between the two ends is
/// an `Option`** — the local agent has no database and no object storage,
/// cortexd has no sandbox status and no outbox backlog. `Option` rather than a
/// type per end: what the caller has to handle is "does this backend have this
/// item", not "which one am I connected to".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    /// What this end is: `"cortexd"` / `"local-agent"`.
    ///
    /// Clients use it to phrase things correctly: when talking to the local
    /// agent, "database not wired" is not a fault, it is simply absent by
    /// design. An old server does not report this field and is treated as
    /// cortexd — the local agent did not exist back then, so there is no risk of
    /// misjudgement.
    #[serde(default = "role_cortexd")]
    pub role: String,
    /// The wire protocol version. See [`crate::PROTOCOL_VERSION`].
    ///
    /// Reported separately from `version`: the latter changes with every
    /// release, whereas this one only moves when the contract changes
    /// incompatibly. The local agent uses it to decide whether to start at all.
    pub protocol: u32,
    /// The oldest peer protocol version this end can still talk to. See
    /// [`crate::MIN_PEER_PROTOCOL`].
    ///
    /// **It has to be reported**: only the server knows that "the server has
    /// dropped support for clients this old", and without it a client can only
    /// connect and then fail inexplicably on some specific request.
    pub min_peer_protocol: u32,
    /// `"ok"` / `"not_wired"` / a specific error. **The local agent does not
    /// have this one.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Which backend object storage runs on: `"s3"` / `"local_fs"` /
    /// `"unavailable"`. Seeing `local_fs` in production is itself an alert:
    /// media is landing only on that one machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_backend: Option<String>,
    /// The authentication mode: `"token"` / `"disabled"`.
    ///
    /// **`/health` itself requires no authentication**, because its consumers
    /// are the Docker HEALTHCHECK and load balancer probes, and those cannot be
    /// given credentials; the immediate consequence of putting auth on it is a
    /// container that stays unhealthy forever, followed by somebody deleting
    /// the HEALTHCHECK.
    ///
    /// The price is that these few fields are visible to anyone who can reach
    /// the port. Taken one at a time: version number, whether the database is
    /// reachable, the object storage backend, and this line — all of them
    /// **deployment shape**, not user data. And this line in particular is
    /// necessary: without it, "did I actually turn authentication on" can only
    /// be answered by going and reading environment variables on the server,
    /// which is exactly the thing one most wants to see remotely at a glance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// The current state of vectorization. **After a model change this is the
    /// only place the problem is visible at a glance.**
    ///
    /// On a model change all four vector queries filter by the new
    /// `embedding_model`, so old facts **drop out of vector retrieval that same
    /// day**; meanwhile BM25, graph traversal and recency keep working, so it
    /// does not look broken, only like "semantic retrieval seems a bit worse
    /// lately". A non-zero `stale` is that happening.
    ///
    /// Old clients may ignore the field; `Option` lets the mock backend (which
    /// has no database to query) still return `/health` as normal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingHealth>,
    /// One line about the tool sandbox. **Only the local agent has it** — on
    /// the server side the startup log carries this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Whether memory is reachable, and how much is backed up. **Only the local
    /// agent has it.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryHealth>,
}

fn role_cortexd() -> String {
    "cortexd".into()
}

/// The local agent's connection state towards the remote memory store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHealth {
    /// The address of the remote cortexd.
    pub remote: String,
    /// Named `reachable` rather than `ok`: the latter would suggest the memory
    /// store itself is healthy, whereas all that was probed here is that it is
    /// alive.
    pub reachable: bool,
    /// How many episodes are still waiting to be written through.
    pub backlog: u64,
}

/// The vectorization section of `/health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingHealth {
    /// The currently configured model identifier, of the form `api:bge-m3` /
    /// `fastembed:…` / `hash-stub-v1`.
    pub model: String,
    /// The number of valid facts that **have** a vector under the current model
    /// and therefore make it into vector retrieval.
    pub covered: i64,
    /// The number of valid facts that only have vectors from some other model
    /// and therefore **do not** make it into vector retrieval.
    ///
    /// This number goes down once the backfiller runs. Staying non-zero for a
    /// long time means backfill was switched off (`CORTEX_REEMBED=off`), or has
    /// been failing all along — the latter shows up in the logs.
    pub stale: i64,
}

// ──────────────────────────── /chat ────────────────────────────

/// How much this turn is allowed to interrupt the user. Aligned with Claude
/// Code's mode menu.
///
/// # Why three levels rather than copying all five
///
/// Claude Code's menu also has Plan (produce a plan before acting) and Auto
/// (let the model decide). Each of those is a feature in its own right — the
/// first needs a whole plan/approve/execute flow, the second needs a
/// classifier — and neither has anything to do with "do we ask". Stuffing them
/// into the same enum would only make this look finished while in fact leaving
/// two empty shells.
///
/// # Why this lives in cortex-proto
///
/// It is simultaneously an HTTP contract (a field of `ChatRequest`) and a type
/// shared by two hosts. Put it in `cortex-agent` and the string the Flutter side
/// serializes has to line up with an internal enum in the agent layer — and
/// "cross-layer literals that must agree" is exactly the kind of convention
/// this repository has repeatedly seen drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Confirm each one: ask for file writes and for command execution, and ask
    /// for anything out of scope. **The default.**
    #[default]
    Ask,
    /// Edit files automatically: writes go unasked, only execution asks. Out of
    /// scope still asks — being out of scope and being risky are two different
    /// things.
    AcceptEdits,
    /// Everything goes: never ask, not even out of scope. Aligned with "Bypass
    /// permissions".
    ///
    /// Deliberately not the default: a config copied from somewhere else, or a
    /// field somebody forgot to send, should not silently grant the agent
    /// unattended execution. Turning it on requires somebody to choose it
    /// explicitly.
    Bypass,
}

/// The request body of one conversational turn.
///
/// # There used to be a `sandbox: bool` here. **Do not add it back.**
///
/// It let the client declare per turn whether "this turn runs in the cloud
/// sandbox". What that produced in the field: a user staring at a switch they
/// do not understand ("cloud sandbox"? what happens if it is off?), sending
/// "take a look at this file for me" with it off, and getting an agent with no
/// file tools — a failure that reads like the agent being broken, not like a
/// switch being off.
///
/// The **server** decides now: if cortexd can reach docker, it runs in the
/// sandbox. If it cannot, it says plainly that this deployment cannot run
/// conversations (cortexd is the memory service; it runs no agent of its own)
/// and points at the two routes that do work. The user deals only with the
/// session; whether there is a container behind it is transparent to them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub session_id: String,
    pub message: String,
    /// The attachments carried by this turn. The hashes must be **already
    /// registered** blobs (via `/blobs`, or `/blobs/presign` +
    /// `/blobs/commit`); the server only creates the association and no bytes
    /// travel down this path.
    ///
    /// Empty by default, so old clients that never send this field keep working.
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// This turn's permission level. Old clients that omit it get
    /// [`PermissionMode::Ask`].
    ///
    /// Carried per turn rather than stored on the session: the user can change
    /// it at the bottom of the chat box at any moment, and once changed the
    /// **next** utterance should already follow the new level. Storing it
    /// server-side means one more synchronization for the client, and when that
    /// synchronization fails what the user sees is "but I *did* switch the
    /// level".
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

/// One `episode_blobs` association — the **upstream** direction
/// (client → server).
///
/// It carries only what the client knows and the server does not. `mime` and
/// `size_bytes` are deliberately absent: they are uniquely determined by the
/// bytes, the server already sniffed/recorded them when the blob was
/// registered, and having the client report them again would only add a "what
/// the client said differs from what is in the bytes" path to handle. For the
/// downstream direction see [`AttachmentDto`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    /// The SHA-256 of an already registered blob (lowercase hex).
    pub hash: String,
    /// A semantic label (image / audio / video / document …). Passed straight
    /// through to the `episode_blobs.kind` column; the schema deliberately does
    /// not enumerate it, so neither does this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The filename the user sees.
    ///
    /// Stored on `episode_blobs` rather than on `blobs`: under content
    /// addressing the same bytes can have several filenames (the same image is
    /// "design-draft.png" today and arrives forwarded as "IMG_2043.png" next
    /// week), so the filename is a property of **this particular reference**.
    ///
    /// `None` by default, so old clients that omit it keep working — they just
    /// cannot show a name on replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// One attachment — the **downstream** direction (server → client).
///
/// It has `mime` and `size_bytes` on top of [`AttachmentRef`]. Before those two
/// were sent down, reopening a session could only render a PDF as
/// "document · a1b2c3d4": the client held nothing but the hash, and finding out
/// what the thing was meant one more request per attachment, with nothing at
/// all to draw until then. The fields are purely additive; old clients may
/// ignore them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentDto {
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `null` = unknown (older data persisted before this change, whose original
    /// filename was never recorded). Clients should fall back to the
    /// "document · <first 8 hex of hash>" presentation.
    pub filename: Option<String>,
    /// The type sniffed from the leading bytes, taken from `blobs.mime`.
    pub mime: String,
    pub size_bytes: i64,
}

/// SSE events. The `type` field is the discriminant; clients dispatch on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// Incremental text. Clients should append rather than replace, otherwise
    /// the view flickers and reflows.
    Delta { text: String },
    /// Which memories were injected this turn — this is where "auditable"
    /// lands in the UI, letting the user see on what grounds the agent answered
    /// the way it did.
    Memory { facts: Vec<FactDto> },
    /// A tool call (the coding scenario).
    Tool {
        name: String,
        summary: String,
        /// The file path this call touched.
        ///
        /// **It has to be optional** — not every tool touches a file
        /// (`memory_search` does not), and handing over an empty string makes
        /// the client draw a file entry pointing at the root directory.
        ///
        /// A field of its own rather than having clients regex it out of
        /// `summary`: the summary is human-readable natural language whose
        /// wording changes at any time; once a client depends on its shape, one
        /// rewording means **silently showing the wrong file** — no error, no
        /// crash, just a pointer to a different file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// What this write changed, for line-by-line coloured rendering.
        /// `None` = nothing worth showing (tools that touch no file, and writes
        /// whose content did not change by a single character).
        ///
        /// A field of its own rather than spliced into `summary`, for exactly
        /// the same reason as `path`: the summary is one human-readable
        /// sentence whose wording changes at any time, and once a client slices
        /// structured content out of it, one rewording means **silently showing
        /// the wrong thing**.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    /// **A high-risk tool call needs the user's confirmation. This turn is
    /// already suspended, waiting for a receipt.**
    ///
    /// Clients should show `preview` to the user verbatim, then
    /// `POST /confirmations` (carrying `token` and `decision`).
    ///
    /// # The ordering of this event relative to [`Self::Tool`] is not guaranteed
    ///
    /// The two travel on different internal channels (`Tool` is bridged from
    /// agent events, this one is emitted directly by the host) and can swap
    /// under concurrency. So this event **carries by itself** everything needed
    /// to make the decision (tool name, risk level, full argument preview) and
    /// depends on no preceding `Tool` event — a client must never render this
    /// confirmation dialog based on "what the last Tool event said".
    ///
    /// # What happens if no receipt arrives
    ///
    /// After `timeout_secs` seconds the server treats it as a **rejection** and
    /// carries on with the turn (the model is told "nobody answered"). The
    /// client's still-open confirmation dialog is void at that point, and a
    /// receipt sent afterwards gets a 404. Deliberately no extra "expired"
    /// event is emitted for this: the client holds `timeout_secs` and can run
    /// the countdown itself, whereas one more event is one more ordering to
    /// handle.
    Confirm {
        /// A one-shot credential, returned verbatim with the receipt. 256 bits
        /// of kernel randomness; unguessable.
        token: String,
        /// The name of the tool about to run.
        tool: String,
        /// The risk level: `"safe"` / `"write"` / `"execute"`. The set of
        /// values is pinned by `cortex_agent::Risk::as_wire` — the same
        /// literals as [`crate::confirm::PendingInfo::risk`], because the two
        /// routes (live SSE and the catch-up list) have to be saying the same
        /// thing.
        ///
        /// The server constructs it as a `&'static str`; here it has to be a
        /// `String`, because **the CLI reads this event back** and a
        /// `&'static str` cannot be deserialized into. That is why the CLI once
        /// wrote a `ChatEvent` of its own — and consequently could never see
        /// the newly added `scope` field.
        risk: String,
        /// The full arguments, for human reading. **Do not truncate before
        /// displaying** — the server already truncated once and marked it
        /// explicitly; a second cut by the client means the user approves the
        /// half he never saw.
        preview: String,
        /// How many seconds until this is treated as a rejection.
        timeout_secs: u64,
        /// The absolute target path of an out-of-scope access. `None` = inside
        /// the workspace, nothing out of scope.
        ///
        /// The UI should use it to say things plainly: "the agent wants to
        /// write `C:\Users\x\Desktop\a.txt`, which is outside the current
        /// workspace". Without it all the user sees is a `path` argument, and
        /// he cannot tell whether that is inside the workspace or on his
        /// desktop — two things whose consequences are far apart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// What this write will turn the file into. `None` = no change worth
        /// showing.
        ///
        /// # This field was very nearly left out here
        ///
        /// The diff was initially added only to `PendingMeta` / `PendingInfo`,
        /// i.e. the **catch-up** route of `GET /confirmations`. But live
        /// confirmation travels on this SSE event — so the dialog never had a
        /// diff, and only the catch-up after a reconnect did.
        ///
        /// Both routes have to carry it: they are two ways of arriving at the
        /// same thing, and the symptom of one missing is "usually absent,
        /// occasionally present", which is harder to track down than being
        /// absent altogether.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    /// Finished, carrying this turn's episode id for traceability.
    Done { episode_id: String },
    /// Something went wrong. Still returned as an SSE event, so that a client
    /// is not left with a truncated stream and no way to tell why.
    Error { message: String },
}

// ────────────────────── Tool confirmation (R11) ──────────────────────

/// The request body of `POST /confirmations` — the upstream receipt.
///
/// # Why the token is in the **body** rather than the path
///
/// `POST /confirmations/{token}` would be more RESTful, but that token is a
/// credential capable of approving a shell execution once, and paths land
/// verbatim in access logs, in reverse-proxy logs, and in `history`. Bodies do
/// not.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConfirmReceipt {
    pub token: String,
    pub decision: ConfirmDecision,
}

/// The user's answer.
///
/// An enum rather than `approve: bool`: a JSON `{"approve": "false"}` (a
/// string) gets leniently parsed as `true` by plenty of client libraries, and
/// the direction of a wrong guess here is **approving a command nobody ever
/// approved**. Two hard-coded literals; a typo is a 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmDecision {
    Allow,
    Deny,
}

#[derive(Debug, Serialize)]
pub struct ConfirmAck {
    /// Always true — a receipt that was not accepted gets a 404 and never
    /// reaches this point.
    pub accepted: bool,
}

/// The response of `GET /confirmations`: the confirmations still awaiting an
/// answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct PendingConfirmations {
    pub pending: Vec<crate::confirm::PendingInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PendingQuery {
    /// Look at only one session's. Omit it to get all of them — a client that
    /// has just reconnected does not yet know which session to care about, and
    /// needs the whole picture before it can decide.
    #[serde(default)]
    pub session_id: Option<String>,
}

// ──────────────────────── Authentication (R9) ────────────────────────

/// The response of `POST /auth/ticket`.
///
/// See cortexd's `auth` module: a browser's `EventSource` / `WebSocket` /
/// `<img>` cannot add request headers, so the credential has to go in the URL;
/// putting a long-lived token there would land it in the logs, so a long-lived
/// token is first exchanged for a short-lived ticket, and only the ticket goes
/// in the URL.
#[derive(Debug, Serialize)]
pub struct TicketResponse {
    pub ticket: String,
    pub expires_in_secs: u64,
}

// ─────────────────────────── Memory ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactDto {
    pub id: String,
    pub statement: String,
    pub predicate: Option<String>,
    pub domain: Option<String>,
    pub confidence: f32,
    /// Event time: when this became true in the real world.
    pub valid_at: Option<String>,
    /// System time: when Cortex came to know it.
    pub created_at: String,
    /// This fact has **as of now** been overturned.
    ///
    /// # Why add a flag rather than simply not returning invalidated facts
    ///
    /// "Everyday retrieval does not return invalidated facts" is **already**
    /// how things are: all four retrieval channels query the `active_facts`
    /// view, so in a retrieval without `as_of` this field is invariably
    /// `false`.
    ///
    /// Where it actually means something is **replay** (`?as_of=`): that route
    /// deliberately goes back to `facts` + `fact_events` and replays the
    /// snapshot as it stood, returning precisely the "things I believed three
    /// months ago that were later overturned" — which is this project's selling
    /// point, and filtering them out would turn replay into a feature that can
    /// only repeat the present. But returning them without marking them is
    /// equally wrong: the user sees a conclusion that no longer holds, looking
    /// in the UI exactly like a current one.
    ///
    /// So neither route chooses "filter"; both choose "hand it over anyway, but
    /// say so" — consistent with [`InjectedMemoryDto::invalidated`], whose
    /// comment says the same thing. This asymmetry is exactly what clients
    /// reported.
    ///
    /// Old clients may ignore the field; `#[serde(default)]` keeps it from
    /// being a breaking change on deserialization too.
    #[serde(default)]
    pub invalidated: bool,
    /// Provenance — open it to see the original conversation that produced this
    /// memory.
    pub source_episode_id: Option<String>,
    /// The source channel: `user_stated` / `conversation` / `derived` /
    /// `tool_output` / `external` / `unknown_legacy`.
    ///
    /// # Why this has to reach the client
    ///
    /// "The user said it themselves" and "the model inferred it from the
    /// conversation" looking identical in the drawer is the one ambiguity this
    /// product can least afford: **"why do you remember this" is its selling
    /// point**, and a user's trust in an inferred fact is inherently different
    /// from their trust in one they stated.
    ///
    /// This column has been written since migration 20260807000006; it was just
    /// that nobody could read it.
    ///
    /// Old clients may ignore it; `#[serde(default)]` keeps it from being a
    /// breaking change.
    #[serde(default)]
    pub source_channel: Option<String>,
    /// The source trust tier, 1 being highest. Its correspondence with
    /// [`Self::source_channel`] is pinned by the schema's
    /// `facts_trust_tier_matches_channel`.
    ///
    /// Sent down separately rather than having the client compute it from the
    /// channel name: that would be a third copy of the mapping (one in Rust,
    /// one in SQL, one in Dart), and nothing at all would constrain the third
    /// one.
    ///
    /// `None` can only mean rows that existed before the column was added
    /// (`unknown_legacy`).
    #[serde(default)]
    pub trust_tier: Option<i16>,
}

impl FactDto {
    /// Convert into the shape injection rendering wants.
    ///
    /// # Why it lives here
    ///
    /// Turning a `FactDto` into text for the model is something three places
    /// have to do: the local agent (`cortex-local::turn`), cortexd's mock
    /// backend, and the outward-facing MCP server. Previously only the first
    /// existed, and it had copied the field shuffling into itself.
    ///
    /// The cost of a second copy is not the duplication itself, it is that
    /// **a missed field raises no error**: add a field to `FactDto` that
    /// participates in rendering (which is how `trust_tier` arrived) and the
    /// copy that missed it simply renders a little less — it compiles, the
    /// tests stay green, and the symptom is "the same fact looks different
    /// depending on which route it came through".
    ///
    /// Placed in `cortex-proto` rather than `cortex-core`: the direction is
    /// proto → core (proto already depends on core), and the reverse would make
    /// core aware of the HTTP contract.
    #[must_use]
    pub fn to_memory_item(&self) -> cortex_core::injection::MemoryItem {
        cortex_core::injection::MemoryItem {
            id: self.id.clone(),
            statement: self.statement.clone(),
            valid_at: self.valid_at.clone(),
            known_since: self.created_at.clone(),
            source_episode_id: self.source_episode_id.clone(),
            domain: self.domain.clone(),
            predicate: self.predicate.clone(),
            confidence: Some(self.confidence),
            source_channel: self.source_channel.clone(),
            trust_tier: self.trust_tier,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MemorySearchQuery {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// Replay by system time: "what did I believe three months ago". Leave it
    /// empty to query the currently valid facts.
    pub as_of: Option<String>,
}

fn default_limit() -> i64 {
    20
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    pub facts: Vec<FactDto>,
    /// Which retrieval channels each result was hit by, for debugging and
    /// observability.
    pub channels: Vec<ChannelHit>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelHit {
    pub fact_id: String,
    pub channels: Vec<String>,
    pub score: f64,
}

// ────────────────────────── episodes ───────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct EpisodeDto {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub text: Option<String>,
    pub occurred_at: String,
    /// The attachments hanging off this message. An empty array rather than an
    /// omission — clients should not have to distinguish "no attachments" from
    /// "this version of the server does not send attachments".
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    /// Which memories were injected this turn — the contents of the "why do you
    /// remember this" drawer.
    ///
    /// Hung on the **user** message (the anchor of a conversational turn; the
    /// assistant one never lands in the database when the model errors out).
    /// Which bubble the client draws the drawer on is up to the client.
    ///
    /// The vast majority of messages have nothing here, so when empty the whole
    /// field is omitted rather than sent as an empty array: on a session with
    /// several hundred messages, several hundred extra `"memories":[]` are pure
    /// waste.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<InjectedMemoryDto>,
    /// Which tools were called this turn. Same omission rule as above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDto>,
}

/// One injected memory as seen during replay.
///
/// The server stored only the `fact_id`; the text here is the **current state**
/// looked up from the database. So when a fact is redacted the drawer follows
/// it into a placeholder, with no separate sweep needed. The price is losing
/// verbatim fidelity — but a fact's statement is itself append-only, so
/// "current" and "at the time" differ only when something has been redacted,
/// which is exactly the occasion on which we want them to differ.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedMemoryDto {
    pub fact_id: String,
    /// `null` = the row for this fact is no longer there. The UI should show
    /// "referenced a memory that is no longer visible" rather than hiding the
    /// entry entirely — hiding it is tampering with the replay.
    pub statement: Option<String>,
    pub domain: Option<String>,
    /// The retrieval channels that hit it (bm25 / vector / graph / episode …).
    pub channels: Vec<String>,
    pub score: Option<f64>,
    /// This fact has been invalidated since it was injected. **Returned
    /// anyway** — "what was relied on at the time no longer holds" is exactly
    /// what an audit most wants to see.
    pub invalidated: bool,
    /// Provenance — open it to see the original conversation that produced this
    /// memory.
    pub source_episode_id: Option<String>,
}

/// One tool call as seen during replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDto {
    pub name: String,
    /// See `path` on [`ChatEvent::Tool`].
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    /// See `diff` on [`ChatEvent::Tool`]. The persisted copy is **already
    /// truncated** (the agent side truncates it; the database CHECK is the last
    /// fence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// A session overview.
///
/// `title` can come from two places: a name the user gave via
/// `PATCH /sessions/{id}`, or (when it has never been renamed) one derived from
/// the first user message. `title_is_custom` makes that difference explicit —
/// which of the two the "rename" input field is prefilled with depends on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDto {
    pub id: String,
    pub title: String,
    /// Whether `title` was set by the user (true) or derived from the first
    /// message (false).
    pub title_is_custom: bool,
    /// The time of the session's first message.
    pub created_at: String,
    /// The time of the last message; the list is sorted by it, descending.
    pub updated_at: String,
    pub message_count: i64,
    /// A summary of the last message, for the list's preview.
    pub preview: Option<String>,
    /// Archived — absent from `GET /sessions` by default.
    ///
    /// **Archiving is not deletion**: messages, attachments and extracted
    /// memories are all untouched, and it can be undone at any time. Calling it
    /// "delete" in the UI is fine, but do not promise "permanently deleted" in
    /// the copy — real destruction is redact / purge, which is a different
    /// route requiring a second confirmation.
    pub archived: bool,
    /// The absolute path of the bound local directory.
    ///
    /// `null` = a pure chat session: file tools **do not** appear in the tool
    /// catalogue given to the model, so the model says outright that it cannot
    /// read files, rather than failing a few calls before giving up.
    ///
    /// Note this is a path on **that one machine**. Multi-device sync pushes the
    /// binding to other devices, where the same path most likely does not exist
    /// — a client encountering a non-existent path should display it as
    /// unbound.
    pub workspace: Option<String>,
    /// The id of the owning project. `null` / absent = ungrouped.
    ///
    /// **It never points at a deleted project**: deleting a project merely
    /// dissolves the grouping, and the server collapses dangling ownership into
    /// ungrouped while computing the final state. Clients can therefore safely
    /// take this id and look the name up in the results of `GET /projects`;
    /// not finding it just means their own list is stale.
    ///
    /// Old clients may ignore it; when empty the whole field is omitted, sparing
    /// every session an extra `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// **Where** this conversation runs.
    ///
    /// # Why the session has to carry this across devices
    ///
    /// Without it, the same session runs silently on both ends while the two
    /// ends point at completely different filesystems: on the web it is a cloud
    /// container's `/workspace`, on the desktop it is some directory on that
    /// machine. The user has the agent write a file on device A, continues the
    /// conversation on device B, and the agent says "there is no such file" —
    /// which is the truth, only in a different world. And nothing in the UI
    /// explains any of this.
    ///
    /// Of the four products surveyed (Claude Code / Codex / Cursor /
    /// OpenHands), not one allows the same session to run separately on two
    /// ends: **the execution environment is part of a session's identity.**
    ///
    /// Old clients that do not know this field ignore it, and their behaviour
    /// falls back to what it is today.
    #[serde(default)]
    pub runtime: SessionRuntimeDto,
}

/// The values of [`SessionDto::runtime`].
///
/// # Why `Local` does not say "which device"
///
/// There is no client device identity on this wire (the `device_id` in session
/// events bears cortexd's own stamp). But that identity **need not be
/// introduced**: a local binding lives in the client machine's
/// `workspaces.json` and is therefore naturally confined to that one machine,
/// so "does this machine have a binding for this session" is itself the device
/// check — and a harder one than an id, since ids lie after a reinstall or a
/// clone.
///
/// The price is being unable to say "on which one", only "it is bound to a
/// directory on another machine".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeDto {
    /// A cloud container workspace. **The default**, and the only one that can
    /// be resumed anywhere.
    #[default]
    Cloud,
    /// Pinned to a local directory on one machine. Other devices can open it to
    /// look, but not to continue.
    Local,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionDto>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListSessionsQuery {
    /// Whether to list archived sessions too. Defaults to false.
    #[serde(default)]
    pub include_archived: bool,
    /// Look only at the sessions in one project. Omit it = all of them
    /// (including ungrouped ones).
    ///
    /// There is deliberately **no** "only ungrouped" value: having one would
    /// mean defining yet another magic string (`""`? `"none"`?), a value that
    /// can never be changed once settled on, and that is forever ambiguous with
    /// "a project id that happens to equal the magic value". A client that
    /// wants that view can filter the full list by `project_id == null` — it
    /// has to render the full list anyway.
    #[serde(default)]
    pub project_id: Option<String>,
}

/// The request body of `PATCH /sessions/{id}`.
///
/// The fields are independent of each other and all optional; only those sent
/// are changed. Several changes in one PATCH are written in **one write
/// transaction** — otherwise "rename + archive" shows up on another device as
/// the renamed-but-not-yet-archived state first, and the list flickers.
///
/// # The three states of workspace
///
/// This field is an `Option<Option<String>>`, whose three values mean three
/// different things:
///
/// | Request body | Meaning |
/// |---|---|
/// | field absent | leave the workspace alone |
/// | `"workspace": null` | **unbind**; the session falls back to pure chat |
/// | `"workspace": "D:/codes/x"` | bind to that directory |
///
/// Expressing unbind with `null` rather than adding an `unbind: true` boolean:
/// two fields would create a "workspace and unbind both sent" combination that
/// has to be defined, and there is no sensible meaning for it.
#[derive(Debug, Default, Deserialize)]
pub struct SessionPatch {
    /// The new title. A blank string is rejected (restoring the derived title
    /// is not supported in this version — see below).
    #[serde(default)]
    pub title: Option<String>,
    /// true = archive, false = unarchive.
    #[serde(default)]
    pub archived: Option<bool>,
    #[serde(default, deserialize_with = "explicit_option")]
    pub workspace: Option<Option<String>>,
    /// The owning project; the three states are exactly those of `workspace`:
    ///
    /// | Request body | Meaning |
    /// |---|---|
    /// | field absent | leave ownership alone |
    /// | `"project_id": null` | **move out of the project**, back to ungrouped |
    /// | `"project_id": "01J…"` | move into that project |
    ///
    /// Moving into a project that does not exist or has been deleted yields a
    /// 400 — accept it silently and the client shows "moved in" while the
    /// session in the list is still ungrouped (the final-state view collapsed
    /// the dangling ownership to NULL), so what the user sees is "I dragged it
    /// in and it bounced back out".
    #[serde(default, deserialize_with = "explicit_option")]
    pub project_id: Option<Option<String>>,
    /// Declare where this session runs. See [`SessionRuntimeDto`].
    ///
    /// Field absent = leave it alone. **There is no `null` state** — "no
    /// execution home" is not a legal state: a session always runs somewhere,
    /// and the default is the cloud.
    ///
    /// The writer is usually the **local agent**, not the UI: the binding
    /// happens at `PUT /local/workspaces/{id}` (which touches the network not
    /// at all), and the agent then immediately syncs up the fact that "this
    /// session now belongs to this machine" — it being the only place that
    /// knows both things at once.
    #[serde(default)]
    pub runtime: Option<SessionRuntimeDto>,
}

/// Distinguish "field present and null" from "field absent".
///
/// serde parses both as `None` by default, and here they have to be told apart:
/// the first means "unbind", the second means "leave it alone". Wrapping in one
/// more `Option` is the standard trick — `deserialize` is only called when the
/// field is actually present, and its absence goes through
/// `#[serde(default)]` to give the outer `None`.
fn explicit_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(d).map(Some)
}

/// The query parameters of `GET /sessions/{id}` — cursor pagination.
///
/// # Why pagination is mandatory, and why it defaults to the **newest**
///
/// It used to be `ORDER BY occurred_at ASC LIMIT 500`. On a session with more
/// than 500 turns, what got dropped was **the newest ones**: the user opens a
/// long session and sees a conversation with no ending, and nothing in the
/// response says it was truncated at all. It now defaults to the newest page,
/// with `before` to page backwards.
#[derive(Debug, Default, Deserialize)]
pub struct SessionDetailQuery {
    /// How many per page. Defaults to [`DEFAULT_EPISODE_PAGE`], capped at
    /// [`MAX_EPISODE_PAGE`].
    #[serde(default)]
    pub limit: Option<i64>,
    /// Page further back: return only messages strictly earlier than this
    /// cursor. The value comes from the previous response's `next_cursor`, and
    /// its format is **opaque** to clients.
    #[serde(default)]
    pub before: Option<String>,
}

/// The default number of messages per page.
///
/// **Deliberately equal to the hard cap of 500 that preceded this change.** An
/// old client sending no parameters therefore sees not one message fewer: it
/// still gets 500, only they are now "the newest 500" instead of "the oldest
/// 500". Casually lowering the default (to, say, 200) would make old clients
/// **see fewer** messages on sessions of 200 to 500 — trading one bug for
/// another. Clients wanting a smaller first screen should pass `limit`
/// explicitly.
pub const DEFAULT_EPISODE_PAGE: i64 = 500;

/// The cap on messages per page.
///
/// A client may ask for fewer, never for more — this cap is insurance for
/// server memory and JSON size, not a knob for clients to turn. It currently
/// equals [`DEFAULT_EPISODE_PAGE`]; the two constants are still kept separate
/// because they answer different questions ("how many when you do not say" vs
/// "how many at most"), and adjusting the default later should not casually
/// move the insurance.
pub const MAX_EPISODE_PAGE: i64 = 500;

/// The detail of one session: the overview plus one page of messages.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: SessionDto,
    /// This page's messages, **in chronological order** (old → new).
    ///
    /// # The reversal happens on the server, not the client
    ///
    /// Pagination naturally runs newest-to-oldest, but rendering wants
    /// chronological order, so somewhere there has to be a reversal. The server
    /// was chosen: this field was already chronological **before** this change,
    /// and clients render straight from the array order. Were it changed to be
    /// sent descending, old clients would raise no error and not crash — they
    /// would simply **draw the whole conversation backwards**, a breaking change
    /// with no error signal whatsoever. The server's cost is one `reverse()`,
    /// and what it buys is old clients working untouched.
    pub episodes: Vec<EpisodeDto>,
    /// There are earlier messages before this page.
    ///
    /// Not inferred from "is `episodes.len()` equal to limit" — when the count
    /// divides exactly, that inference costs one extra empty page, and clients
    /// mostly read an empty page as "that's the end"; mixing the two judgements
    /// produces "sometimes you cannot page back to the earliest few".
    pub has_more: bool,
    /// Pass it as the next request's `before` to fetch an earlier page. `null`
    /// when `has_more` is false.
    pub next_cursor: Option<String>,
}

// ─────────────────────────── /projects ─────────────────────────

/// A project — a grouping container for sessions.
///
/// # What it deliberately does not have
///
/// No `archived`: a project is a lightweight grouping to begin with, and adding
/// a layer of "archived projects" on top would only give the UI one more filter
/// nobody uses. To put one away, delete it — and deleting a project **deletes
/// no content**, its sessions merely go back to being ungrouped (see
/// [`ProjectsResponse`]).
///
/// No `description` / `instructions`: that is the "project-level system prompt"
/// feature, which is a different thing from grouping. Leaving a
/// forever-empty field here now would only make all three front ends write its
/// rendering and editing first, and then discover the semantics had not been
/// settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDto {
    pub id: String,
    pub name: String,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// The number of sessions in this project that **will appear in the session
    /// list**: archived ones do not count, and neither do ones with not a
    /// single message yet. The definition matches
    /// `GET /sessions?project_id=` — if the two disagree, the user sees "3
    /// sessions", clicks in, and finds 2.
    pub session_count: i64,
}

/// The response of `GET /projects`.
///
/// **Deleted projects are not included.** Deleting a project is a tombstone
/// event: the project vanishes from the list, its sessions become ungrouped,
/// and not one message, attachment or extracted memory is lost — so do not
/// phrase it as "permanently deleted" in the UI copy; that is what redact /
/// purge are for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsResponse {
    pub projects: Vec<ProjectDto>,
}

/// The request body of `POST /projects`.
#[derive(Debug, Deserialize)]
pub struct NewProjectRequest {
    /// Must be non-empty after trimming and no longer than 100 characters.
    pub name: String,
}

/// The request body of `PATCH /projects/{id}`.
///
/// Renaming is currently the only thing to do. Still PATCH with an optional
/// field rather than `POST /projects/{id}/rename`: when a second mutable
/// attribute arrives, the former means adding one field, the latter means
/// opening another path and writing authorization and tenant resolution all
/// over again.
#[derive(Debug, Default, Deserialize)]
pub struct ProjectPatch {
    #[serde(default)]
    pub name: Option<String>,
}

// ──────────────────────────── /blobs ────────────────────────────

/// A registered binary object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobDto {
    /// SHA-256, lowercase hex. This is both the content identifier and the path
    /// parameter used to fetch it back.
    pub hash: String,
    /// Sniffed from the leading bytes — **not** what the client declared.
    pub mime: String,
    pub size_bytes: i64,
    /// This content **already existed** (no bytes and no `blobs` row were added
    /// this time). For observability and UI hints only; do not use it to decide
    /// whether to proceed — idempotence is guaranteed by content addressing
    /// itself, not by this boolean.
    pub deduplicated: bool,
    /// The position of this `blobs` row in the sync total order. Clients use it
    /// to know whether they have caught up.
    ///
    /// `null` means **no row was added** this time (the content had long since
    /// been registered). No number is fabricated: a client handed a
    /// too-large cursor believes it has caught up, and that is a permanent
    /// loss of rows.
    pub seq: Option<i64>,
}

/// The request preceding a direct upload.
#[derive(Debug, Deserialize)]
pub struct BlobPresignRequest {
    /// The SHA-256 of the content. **The client must compute it first** — under
    /// content addressing there is no "upload first, decide the key later"
    /// step; the key *is* the content.
    pub hash: String,
}

#[derive(Debug, Serialize)]
pub struct BlobPresignResponse {
    pub url: String,
    /// Always `PUT`. Spelled out so clients do not have to guess, and to leave
    /// room for switching to a POST form later.
    pub method: &'static str,
    pub expires_in_secs: u64,
    /// The content is already in object storage — the client can skip the
    /// upload and call `/blobs/commit` directly. This is the single largest
    /// bandwidth saving content addressing gives mobile clients: forwarding the
    /// same image does not have to send it again.
    pub already_uploaded: bool,
}

/// Registration after a direct upload completes: writing the `blobs` row (step
/// two).
#[derive(Debug, Deserialize)]
pub struct BlobCommitRequest {
    pub hash: String,
    /// The byte count as declared by the client. See the note in
    /// `AppState::commit_blob` on why the client is trusted here.
    pub size_bytes: i64,
    /// The declared MIME, used only as a fallback when sniffing fails.
    #[serde(default)]
    pub mime: Option<String>,
}

/// One direct download URL.
#[derive(Debug, Serialize)]
pub struct BlobUrlResponse {
    pub url: String,
    pub expires_in_secs: u64,
}

// ──────────────────────────── /sync ────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SyncQuery {
    /// The cursor the client holds. It addresses sync_log's single total order,
    /// which spans every table.
    #[serde(default)]
    pub since: i64,
    #[serde(default = "default_sync_limit")]
    pub limit: i64,
}

fn default_sync_limit() -> i64 {
    500
}

#[derive(Debug, Serialize)]
pub struct SyncResponse {
    /// The `since` to pass on the next fetch.
    pub cursor: i64,
    pub records: Vec<SyncRecord>,
    /// Whether there is more; clients use it to decide whether to keep pulling.
    pub has_more: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncRecord {
    pub seq: i64,
    pub table: String,
    pub id: String,
    /// The business row's contents. Replaying in log order naturally satisfies
    /// foreign-key ordering.
    pub payload: serde_json::Value,
}

// ───────────────────────── /ws live sync ─────────────────────────

/// WebSocket downstream events. The `type` field is the discriminant, following
/// the same convention as [`ChatEvent`].
///
/// # Why only a signal is pushed, never the data
///
/// Stuffing the changed rows straight into the WS message looks like it saves a
/// round trip, but it buys little and the costs are very real:
///
/// 1. **Two serializations will drift.** The same row would have two encoding
///    paths, REST ([`SyncRecord`]) and WS. They will fall out of step on some
///    field change, and the symptom is "only the live-pushed record has the
///    wrong fields" — with the polling path testing perfectly, the hardest
///    class of bug there is.
/// 2. **Reconnection still has to catch up by cursor.** Pushes during a WS
///    outage are lost forever, so the client has to be able to catch up from its
///    own cursor no matter what. Given that this path must exist and must be
///    correct, pushing the full payload merely adds a bypass that is **used
///    normally and unused when things go wrong** — and bugs on that bypass are
///    masked precisely under normal conditions.
/// 3. **A push is only meaningful if it is ordered and complete.** Guaranteeing
///    that means keeping a per-connection cursor and retransmission buffer on
///    the server, i.e. reimplementing `/sync` on top of WS.
///
/// So the downstream only says "the server's cursor is now at N", and the
/// client takes **its own** cursor to `GET /sync?since=`. The `cursor` in the
/// event is only for the client to judge whether it has caught up and to
/// display; it **must never be used as the next fetch's `since`** (that would
/// skip the range the client has not pulled yet).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    /// The connection is established. Carries the server's current cursor, so
    /// the client can immediately tell how far behind it is.
    Hello { cursor: i64, version: &'static str },
    /// There is new data; the server's cursor has advanced to `cursor`.
    Bump { cursor: i64 },
    /// A bump may have been missed in between (the notification path between
    /// the server and Postgres broke, or this connection is consuming too
    /// slowly). Semantically identical to `bump`; it is a separate type so that
    /// clients can measure "normal increments" apart from "possibly a gap" —
    /// the latter appearing frequently is an operational signal.
    Resync { cursor: i64 },
}

// ──────────────────────────── Errors ─────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states of `workspace` have to be told apart.
    ///
    /// serde parses both "field absent" and "field is null" as `None` by
    /// default, and here the two mean opposite things: the former is "leave the
    /// workspace alone", the latter is "unbind". Conflate them and every time a
    /// client changes a title it also unbinds the workspace — a bug that only
    /// surfaces when the user has both bound a workspace and renamed the
    /// session.
    #[test]
    fn workspace_distinguishes_absent_from_null() {
        let absent: SessionPatch =
            serde_json::from_str(r#"{"title":"改个名"}"#).expect("should parse");
        assert!(
            absent.workspace.is_none(),
            "an absent field has to mean \"leave the workspace alone\""
        );

        let unbind: SessionPatch =
            serde_json::from_str(r#"{"workspace":null}"#).expect("should parse");
        assert_eq!(
            unbind.workspace,
            Some(None),
            "an explicit null has to mean \"unbind\", and must not be conflated with an absent field"
        );

        let bind: SessionPatch =
            serde_json::from_str(r#"{"workspace":"D:/codes/x"}"#).expect("should parse");
        assert_eq!(bind.workspace, Some(Some("D:/codes/x".into())));
    }

    /// The three states of `project_id` work like `workspace`'s, and conflating
    /// them is just as silent: every time the user changes a title, the session
    /// gets kicked out of its project along the way.
    #[test]
    fn project_id_distinguishes_absent_from_null() {
        let absent: SessionPatch =
            serde_json::from_str(r#"{"title":"改个名"}"#).expect("should parse");
        assert!(
            absent.project_id.is_none(),
            "an absent field has to mean \"leave ownership alone\" — read as \"move out of the \
             project\", renaming would kick the session out of its project"
        );

        let out: SessionPatch =
            serde_json::from_str(r#"{"project_id":null}"#).expect("should parse");
        assert_eq!(
            out.project_id,
            Some(None),
            "an explicit null has to mean \"move out of the project\", and must not be conflated \
             with an absent field"
        );

        let into: SessionPatch =
            serde_json::from_str(r#"{"project_id":"01JPROJECT0000000000000001"}"#)
                .expect("should parse");
        assert_eq!(
            into.project_id,
            Some(Some("01JPROJECT0000000000000001".into()))
        );
    }

    #[test]
    fn an_empty_patch_parses_and_changes_nothing() {
        // An empty body must not fail to parse — a failure would collapse "the
        // server does not recognize the field I sent" and "I sent nothing at
        // all" into the same 400, leaving no clue to work from.
        let p: SessionPatch = serde_json::from_str("{}").expect("an empty object should parse");
        assert!(
            p.title.is_none()
                && p.archived.is_none()
                && p.workspace.is_none()
                && p.project_id.is_none()
        );
    }

    #[test]
    fn list_query_without_project_means_every_session() {
        let q: ListSessionsQuery =
            serde_json::from_str("{}").expect("missing parameters should take the defaults");
        assert!(
            q.project_id.is_none(),
            "omitting project_id has to mean \"every session\", not \"ungrouped\" — the moment \
             the default is the latter, old clients suddenly see only ungrouped sessions"
        );
    }

    #[test]
    fn list_query_defaults_to_hiding_archived() {
        // JSON instead of a query string: what is doing the work here is
        // `#[serde(default)]`, which is independent of the particular
        // Deserializer, and serde_urlencoded is not a dependency of this crate.
        let q: ListSessionsQuery =
            serde_json::from_str("{}").expect("missing parameters should take the defaults");
        assert!(
            !q.include_archived,
            "with no parameters, archived sessions have to be hidden — the product meaning of \
             archiving is precisely disappearing from the list"
        );
    }
}
