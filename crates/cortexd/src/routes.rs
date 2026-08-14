//! HTTP routes.
//!
//! Handlers do protocol conversion only (HTTP ↔ domain types); all business
//! logic sinks into cortex-memory / cortex-agent / cortex-store.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures::stream::Stream;
use tokio_stream::StreamExt as _;

use crate::blobs::{DIRECT_UPLOAD_LIMIT, RangeSpec, parse_range};
use crate::dto::*;
use crate::state::AppState;
use cortex_proto::episodes::{EpisodeAck, NewEpisodeRequest};
use cortex_proto::llm::{LlmStreamChunk, LlmStreamError, LlmStreamRequest};

/// Declare every protected route — **the registration and the inventory come
/// from one declaration**.
///
/// # Why this has to be a macro
///
/// "Which routes sit behind the authentication middleware" previously had no
/// test capable of catching it going wrong, and the first instinct for adding
/// one (hand-writing a table of paths to assert on) turns one easy-to-forget
/// place into two: whoever adds a route equally forgets to update that table, so
/// the new route is neither protected nor tested, while compilation, clippy and
/// the existing tests are **all green**.
///
/// This uses the same technique as `cortex-store`'s `table::ALL`: the macro
/// generates [`PROTECTED_ROUTES`] while it expands the registration chain, and
/// `mod tests` uses it to hit the real `router()` entry by entry. Adding a route
/// necessarily adds it to the inventory; it cannot be forgotten.
///
/// # The expansion order is the other half of this fence; leave it alone
///
/// Every `.route()` comes first and `.route_layer()` is the final statement —
/// axum's `route_layer` **only covers routes registered before it**. If somebody
/// moves it into the middle, the routes after it lose authentication while
/// remaining in [`PROTECTED_ROUTES`], so
/// `every_protected_route_rejects_anonymous_requests` goes red on the spot.
///
/// `$state` is **the name the caller binds that `AppState` to**.
///
/// It has to be named by the caller and cannot be hard-coded inside the macro:
/// macro_rules hygiene makes an identifier created inside the macro invisible at
/// the call site, so writing `state` in a handler expression would be "cannot
/// find this variable". The `/mcp` entry needs it to construct a tower service,
/// making it the first route that genuinely requires it.
macro_rules! protected_routes {
    ($state:ident; $( $path:literal [$($method:ident),+ $(,)?] => $handler:expr ),+ $(,)?) => {
        /// Every route that **should** require authentication, from the same
        /// source as the registration chain above.
        ///
        /// The `{…}` in a path is an axum parameter placeholder; the tests
        /// substitute a concrete probe value.
        #[cfg(test)]
        const PROTECTED_ROUTES: &[(&str, &[axum::http::Method])] = &[
            $( ($path, &[$(axum::http::Method::$method),+]) ),+
        ];

        fn protected_router($state: AppState) -> Router<AppState> {
            Router::new()
                $( .route($path, $handler) )+
                // route_layer: applies only to routes that **matched**. A
                // non-matching path still 404s and is not judged 401 first —
                // the latter would make "the path was typed wrong" and "the
                // credential is wrong" look identical to the client.
                //
                // This must be the final statement; reasoning in the macro's
                // documentation
                .route_layer(axum::middleware::from_fn_with_state(
                    $state,
                    crate::auth::require,
                ))
        }
    };
}

protected_routes! {
    state;
    "/auth/ticket" [POST] => post(issue_ticket),
    // Bring-your-own API key. Three actions on one path: read status / store /
    // withdraw
    "/settings/llm-key" [GET, PUT, DELETE] => get(crate::byo_key::get)
        .put(crate::byo_key::put)
        .delete(crate::byo_key::delete),
    // There is **no** `/chat` here, and no `/sandbox/*`. Those belong to
    // `cortex-agentd` — that process has docker, this one does not. The routing
    // split happens at the edge (nginx in dev, traefik in prod) and inside no
    // service: letting cortexd know where agentd lives would leave this half
    // carrying an "agent service address" setting, and whoever deploys it
    // standalone has no agentd at all.
    //
    // There is **no** `/confirmations` here either. Tool confirmation belongs to
    // the agent, and the agent lives in a different process: the desktop client
    // asks its local `cortex-local`, and the in-container turn does not ask at
    // all (out-of-bounds paths are refused outright, which is the semantics of
    // `ExecEnvironment::Container` on the agent side).
    //
    // cortexd used to have one too, serving its own in-process agent. Once that
    // agent was deleted this ledger had no producer left — keeping it would
    // mean keeping an endpoint that forever returns an empty list and forever
    // 404s, which is this repository's number-one recurring ailment: **built and
    // never called**.
    //
    // Delegated credentials: sign a short-lived key for an **untrusted caller**,
    // bound to one session. See `cortex_proto::delegate`'s module header — this
    // is cortexd's own inbound authorization policy, not sandbox orchestration;
    // whether a container or something else receives the key is not its concern.
    //
    // Issuing uses **the caller's own bearer**, so it can only issue for "the
    // user who handed it that token" and creates no new privilege.
    "/delegated-tokens" [POST, DELETE] => post(delegate).delete(revoke_delegation),
    "/memory/search" [GET] => get(memory_search),
    // Memory's outward-facing MCP frontage. See `crate::mcp`'s module header.
    //
    // **It must be on this list**: this route can read a person's entire memory,
    // and on the public side it is an unauthenticated read/write port into
    // memory that manifests in no way whatsoever — no error, no alert, just
    // "anyone can read your memory". The
    // `every_protected_route_rejects_anonymous_requests` test below is the only
    // thing capable of catching it.
    //
    // The three methods are what the streamable-http transport needs: POST to
    // send messages, GET to open the SSE return channel, DELETE to end the
    // session. `on_service` turns rmcp's tower Service into a `MethodRouter`, so
    // this route's registration shape is **identical** to every other route —
    // using `route_service` would bypass this macro, and with it the coverage
    // test.
    //
    // **It must be `on_service` and not `any_service`**: the latter opens the
    // door to every method, so the coverage test's probe ("an undeclared method
    // should give 405") would travel all the way into the MCP service and come
    // back with a 400. That probe exists to prove "the path genuinely matched",
    // and once a 400 displaces it, the 401 assertion right after it becomes
    // vacuous — and that assertion is the whole point.
    "/mcp" [POST, GET, DELETE] => axum::routing::on_service(
        axum::routing::MethodFilter::POST
            .or(axum::routing::MethodFilter::GET)
            .or(axum::routing::MethodFilter::DELETE),
        crate::mcp::service(state.clone()),
    ),
    // The LLM proxy. The local agent takes this path by default — the API key
    // lives in exactly one place on the server, so it does not have to be
    // configured on every device. It **touches no memory and writes to no
    // database**; it is pure forwarding
    "/llm/stream" [POST] => post(llm_stream),
    // The local agent writing a turn back. The GET below is replay; the two
    // share a path prefix but run in opposite directions — written on one line,
    // "which method belongs to which handler" would have to be read positionally
    "/episodes" [POST] => post(write_episode),
    "/episodes/{id}" [GET] => get(get_episode),
    "/sessions" [GET] => get(list_sessions),
    // PATCH rather than PUT: the client sends only the fields it wants changed,
    // and anything it does not send stays as it was. PUT means wholesale
    // replacement, which would force the client to GET first and send everything
    // back — and whatever another device changed between those two requests
    // gets quietly rolled back by that PUT
    "/sessions/{id}" [GET, PATCH] => get(get_session).patch(patch_session),
    // Projects — the grouping container for sessions.
    //
    // DELETE here is literally accurate: the project itself really is gone. Using
    // a different verb from "archive a session" is deliberate — deleting a
    // project **does not delete content**: the sessions inside revert to
    // ungrouped, with every message and memory intact
    "/projects" [GET, POST] => get(list_projects).post(create_project),
    "/projects/{id}" [PATCH, DELETE] => axum::routing::patch(patch_project)
        .delete(delete_project),
    // axum's default body limit is 2 MiB — far too tight for "direct upload of
    // small files" (one casual phone photo exceeds it). Widened to
    // DIRECT_UPLOAD_LIMIT; anything larger should go through /blobs/presign
    // straight to object storage without relaying through the server
    "/blobs" [POST] => post(upload_blob).layer(DefaultBodyLimit::max(DIRECT_UPLOAD_LIMIT)),
    // Import from the web client: a browser cannot read the disk, so
    // conversations.json has to be uploaded first. The desktop client does not
    // take this path (its file is local and the local agent reads it directly).
    // The size ceiling is enforced by crate::import itself with a streaming
    // counter, so axum's default gate is removed entirely here — leaving it in
    // would only cut the connection halfway through the read, by which point the
    // spool file is already half written.
    //
    // "Who am I" requires proving who you are first, so it lives on the
    // protected side. login / refresh / logout / register are on the public
    // side; reasoning in `public_routes`
    "/auth/me" [GET] => get(crate::accounts::whoami),
    // "How much do I have left". On the protected side — it answers for the
    // usage of this particular person
    "/auth/usage" [GET] => get(crate::accounts::usage),
    "/import/upload" [POST] => post(crate::import::upload).layer(DefaultBodyLimit::disable()),
    "/import/preview" [POST] => post(crate::import::preview),
    "/import/run" [POST] => post(crate::import::run),
    "/blobs/presign" [POST] => post(presign_blob),
    "/blobs/commit" [POST] => post(commit_blob),
    "/blobs/{hash}" [GET] => get(get_blob),
    "/blobs/{hash}/url" [GET] => get(get_blob_url),
    "/sync" [GET] => get(sync),
    "/ws" [GET] => get(crate::ws::handler),
    // The **index** for sandbox workspace snapshots. The bytes go through the
    // existing `/blobs`.
    //
    // Taking a snapshot needs docker (to export the tar); storing one needs the
    // database and object storage — only agentd has the former, only this side
    // has the latter. So the cut runs down the middle: agentd produces the tar
    // and hands it over, and this side merely records a row. See
    // `crate::snapshots`'s module header.
    "/sandbox-snapshots" [GET, POST] => get(crate::snapshots::list).post(crate::snapshots::record),
}

/// The route table.
///
/// # The shape of authentication: exemptions are expressed by **not attaching
/// # the middleware**, never by a path allow-list
///
/// "Check `path == "/health"` inside the middleware and let it through" is the
/// single most classic failure point of this kind of code: whoever adds a new
/// route will not remember to update that table, and the direction the omission
/// falls in is **allow by default**. So this splits into two `Router`s: the
/// `public` one holds `/health` and friends, and everything else is generated by
/// [`protected_router`] with a single [`crate::auth::require`] layer wrapped
/// around it. A newly added route lands on the protected side by default — the
/// failure direction is inverted.
///
/// # Only the `/health` `.route()` is allowed inside this function body
///
/// Every other route goes into the `protected_routes!` macro above. Both ways of
/// writing one here end badly: adding it to `public` means **no authentication
/// at all**; adding it after `protected_router(...)`'s return value (i.e. after
/// `route_layer`) is equally unauthenticated, and more insidious. The test
/// `router_registers_no_routes_outside_the_macro` reads this file's own source
/// to guard this — it is the only means of catching "a route registered outside
/// the macro".
pub fn router(state: AppState) -> Router {
    let mut public = Router::new();
    for (path, method_router) in public_routes() {
        public = public.route(path, method_router);
    }

    public
        .merge(protected_router(state.clone()))
        .with_state(state)
}

/// The handful of routes that need no authentication, **as one inventory**.
///
/// # Why each of them has to be on the public side
///
/// - `/health`: its consumers are Docker HEALTHCHECK and load balancer probes,
///   neither of which can be given a credential. The direct consequence of
///   requiring authentication is a container that stays unhealthy forever, and
///   then somebody deletes the HEALTHCHECK. What it returns is entirely
///   **deployment shape**, not user data (see `Health::auth`).
/// - `/auth/login`, `/auth/register`: logging in is what you do when you do not
///   yet have a credential. Requiring one in order to log in is a deadlock.
/// - `/auth/refresh`, `/auth/logout`: what they carry is a **refresh token**,
///   which is itself the credential — verified inside the handler (digest
///   comparison plus replay detection). Also requiring an unexpired access token
///   would make "renew automatically after expiry" available only while
///   unexpired.
///
/// # Before adding one
///
/// This function is the **only** unauthenticated entrance to the whole service.
/// The correct way to add something is to first write a paragraph above saying
/// "why this one cannot require a credential" — if you cannot write one, it
/// belongs in `protected_routes!`. `the_public_list_stays_short` goes red when
/// this inventory grows, forcing whoever changed it back here to read this.
fn public_routes() -> Vec<(&'static str, axum::routing::MethodRouter<AppState>)> {
    vec![
        ("/health", get(health)),
        ("/auth/login", post(crate::accounts::login)),
        ("/auth/refresh", post(crate::accounts::refresh)),
        ("/auth/logout", post(crate::accounts::logout)),
        ("/auth/register", post(crate::accounts::register)),
    ]
}

// ─────────────────────────── /health ───────────────────────────

/// The liveness probe. **The only endpoint that needs no authentication**;
/// reasoning in [`Health::auth`].
async fn health(State(st): State<AppState>) -> Json<Health> {
    Json(Health {
        status: st.status().into(),
        version: cortex_core::VERSION.into(),
        role: "cortexd".into(),
        protocol: cortex_proto::PROTOCOL_VERSION,
        min_peer_protocol: cortex_proto::MIN_PEER_PROTOCOL,
        database: Some(st.database_status().await),
        blob_backend: Some(st.blob_backend().into()),
        auth: Some(st.auth_mode().as_str().into()),
        embedding: st.embedding_health().await,
        // The server has neither of these: sandbox status is carried by the
        // startup log, and the memory database is this process itself
        sandbox: None,
        memory: None,
    })
}

// ──────────────────── /auth/ticket (R9) ────────────────────

/// Exchange the long-lived token for a short-lived ticket.
///
/// For connections that **cannot set request headers**: the browser's
/// `EventSource`, `WebSocket`, `<img src>`. The ticket travels as `?ticket=` and
/// expires after 60 seconds. See [`crate::auth`].
async fn issue_ticket(State(st): State<AppState>) -> Json<TicketResponse> {
    Json(TicketResponse {
        ticket: st.ticket_book().issue(),
        expires_in_secs: crate::auth::TICKET_TTL.as_secs(),
    })
}

// ─────────────────── delegation scope ──────────────────────────

/// This session's sandbox scope: whose it is, and which project.
///
/// # A missing project is treated as ungrouped rather than an error
///
/// The session row is created along with the first episode, so **the first turn
/// of a new session necessarily finds nothing** (the same thing shows up on the
/// `cortex-local` side as a 404; see "the 404 on a new session's first turn" in
/// the roadmap). A new session has not been moved into any project yet, so
/// ungrouped is the correct answer.
///
/// An unreachable tenant database is likewise treated as ungrouped: at that
/// point the thing that should raise an error is the actual read or write that
/// follows, not the step "decide which container to use" — raising it here
/// makes the error message point at something entirely unrelated to the cause.
async fn delegation_scope(
    st: &AppState,
    owner: &str,
    session_id: &str,
) -> (
    crate::delegated_token::DelegatedScope,
    cortex_store::SessionRuntime,
) {
    // The project and the runtime assignment come from **the same row**.
    // Queried separately, the two decisions would see different snapshots under
    // concurrent modification, and the symptom is "occasionally lands in the
    // wrong container" — there is nothing harder to reproduce than that
    let state = match st.tenant_store_for_user(owner).await {
        Ok(store) => store.session_state(session_id).await.ok().flatten(),
        Err(e) => {
            tracing::debug!(owner = %owner, error = %e, "could not reach the tenant database; \
                 the sandbox is treated as ungrouped");
            None
        }
    };
    let runtime = state
        .as_ref()
        .map_or(cortex_store::SessionRuntime::Cloud, |s| s.runtime);
    (
        crate::delegated_token::DelegatedScope {
            owner: owner.to_owned(),
            session_id: session_id.to_owned(),
            project: state.and_then(|s| s.project_id),
        },
        runtime,
    )
}

/// Sign a delegated credential for this scope — **reuse an existing one, and
/// only sign a new one when there is none**.
///
/// # Why reuse instead of signing one per turn
///
/// A token's lifetime **follows the container**, not the turn. The container's
/// inbound authentication uses the token in its startup env (`cortex-local`
/// shares one token between inbound and outbound). Signing a new one each turn
/// means the second turn's proxied request is a 401 — and that error reads like
/// "the sandbox is broken". The real-machine symptom "first turn works, second
/// turn 401s" was exactly this.
///
/// The reuse criterion is the **scope key**, not the owner: when the same user
/// switches to a different project, that is a different container and a
/// different token. Looking up by owner would hand project A's token to project
/// B's container, which then comes up and answers normally, only 401ing on the
/// second turn.
///
/// # Why this is its own function
///
/// It has two callers: cortexd's own routes that touch containers, and
/// `POST /delegated-tokens` (the one the agent orchestrator uses). **They must
/// share one implementation** — the symptom of the issuing rules drifting apart
/// is "a container that came in via the other path 401s on its second turn", and
/// those two paths serve different populations of users.
fn delegate_token(st: &AppState, scope: crate::delegated_token::DelegatedScope) -> String {
    let key = scope.key();
    match st.delegations().find_by_key(&key) {
        Some(t) => {
            st.delegations().rebind(&t, &scope.session_id);
            t
        }
        None => st.delegations().issue(scope),
    }
}

/// `POST /delegated-tokens` — sign the caller a key bound to one session.
///
/// See [`cortex_proto::delegate`]'s module header: this is **the memory
/// service's inbound authorization**, not sandbox orchestration. The owner comes
/// from the credential, the project and runtime assignment come from the session
/// row, and all three are returned at once.
async fn delegate(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<cortex_proto::delegate::DelegateRequest>,
) -> Result<Json<cortex_proto::delegate::Delegation>, ApiError> {
    let owner = crate::accounts::current_user(&st, &headers).await;
    let (scope, runtime) = delegation_scope(&st, &owner, &req.session_id).await;
    let scope_key = scope.key();
    // The quota is **reported**, never enforced here: this key is not used only
    // for conversation, and listing files or downloading the workspace with it
    // spends not a single token. See `Delegation::quota_exhausted`
    let quota_exhausted = st.enforce_quota(&owner).await.is_err();
    Ok(Json(cortex_proto::delegate::Delegation {
        token: delegate_token(&st, scope),
        owner,
        scope_key,
        runtime: match runtime {
            cortex_store::SessionRuntime::Local => cortex_proto::dto::SessionRuntimeDto::Local,
            cortex_store::SessionRuntime::Cloud => cortex_proto::dto::SessionRuntimeDto::Cloud,
        },
        quota_exhausted,
    }))
}

/// `DELETE /delegated-tokens` — take back a key that was never put to use.
///
/// Idempotent: revoking one that does not exist (forged, already expired,
/// somebody else's) also returns 204. Reporting those separately would turn
/// "guess a token and see" into a usable probe.
async fn revoke_delegation(
    State(st): State<AppState>,
    Json(req): Json<cortex_proto::delegate::RevokeRequest>,
) -> StatusCode {
    st.delegations().revoke(&req.token);
    StatusCode::NO_CONTENT
}

// ───────────────────────── /llm/stream ─────────────────────────

/// The LLM proxy — forward one streaming call to the provider verbatim.
///
/// The local agent takes this path by default: the API key lives in exactly one
/// place on the server, so it does not have to be configured on every device.
/// Anyone who wants to switch models, switch providers, or use their own relay
/// without touching the server simply configures the local side to connect
/// directly.
///
/// # The two classes of error take two different paths, and that is not an
/// # inconsistency
///
/// Failing **before the stream is established** (the mock backend has no
/// provider, wrong credentials, an invalid model name) → an ordinary HTTP status
/// code. Not a single byte has gone out yet, so what the client should see is a
/// failed request.
///
/// Failing **after the stream is established** (rate limiting, context too long,
/// the provider 5xxing partway through) → an `event: error` on the SSE stream.
/// By then the 200 and the response headers went out long ago and the status
/// code cannot be changed; moreover, several hundred tokens may already have
/// reached the user's eyes, and a silent truncation would leave the client
/// unable to distinguish "it finished" from "it broke".
async fn llm_stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LlmStreamRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Every model call from the desktop client goes through here, which makes it
    // the place the quota most needs to hold. Check **before issuing**: deducting
    // afterwards means the final call always overshoots, and that call might be
    // an import of six thousand calls
    let user = crate::accounts::current_user(&st, &headers).await;

    // Someone with their own key uses it and does not count against the quota,
    // so **look for a key first and only then decide whether to check the
    // quota**. The other order would lock someone who supplied their own key out
    // of their own allowance — while the quota message says, in so many words,
    // that supplying your own key exempts you from it
    let tenant = st.tenant(&headers).await?;
    let own = st.own_key(&tenant).await;
    if own.is_none() {
        st.enforce_quota(&user).await?;
    }

    let upstream = st
        .llm_stream(
            req,
            own.as_ref().map(|k| {
                (
                    k.provider.as_str(),
                    k.api_key.as_str(),
                    k.base_url.as_deref(),
                )
            }),
        )
        .await?;
    let used_own_key = own.is_some();

    // Accounting: every frame in the stream carries a `ProviderUsage`, and the
    // **cumulative value on the last frame** is this call's total. So keep the
    // latest one while forwarding and write a row when the stream ends.
    //
    // The database write is not awaited here: that would insert a database round
    // trip into the path where the user is waiting for a reply. It is recorded
    // after the stream ends instead — the consequence of missing a record is
    // only that this one call does not count against the quota, while the
    // consequence of blocking is that every conversation is a bit slower
    let usage_seen = std::sync::Arc::new(std::sync::Mutex::new((0_i64, 0_i64, String::new())));

    let seen = std::sync::Arc::clone(&usage_seen);
    let stream = upstream.map(move |item| {
        let ev = match item {
            Ok((message, usage)) => {
                // Only frames carrying usage update it. **Take the latest,
                // never sum** — the provider reports a running cumulative
                // total, and summing would multiply the figure several times
                // over
                if let (Some(pu), Ok(mut g)) = (usage.as_ref(), seen.lock()) {
                    g.0 = i64::from(pu.usage.input_tokens.unwrap_or(0));
                    g.1 = i64::from(pu.usage.output_tokens.unwrap_or(0));
                    g.2 = pu.model.clone();
                }
                let chunk = LlmStreamChunk { message, usage };
                // A serialization failure is also emitted as an in-stream
                // error, for the same reason as above: the status code can no
                // longer be changed at this point, and silently dropping an
                // item would leave the local side with a conversation missing
                // its middle, with no way whatsoever to notice
                match serde_json::to_string(&chunk) {
                    Ok(json) => return Some(Ok(Event::default().data(json))),
                    Err(e) => LlmStreamError {
                        kind: "execution_error".into(),
                        message: format!("serialization failed on the proxy side: {e}"),
                        retry_delay_ms: None,
                        top_up_url: None,
                        category: None,
                    },
                }
            }
            Err(e) => LlmStreamError::from_provider(&e),
        };
        let json = serde_json::to_string(&ev)
            .unwrap_or_else(|_| r#"{"kind":"execution_error","message":"internal"}"#.to_string());
        Some(Ok(Event::default().event("error").data(json)))
    });

    // Record this call's usage once the stream has run out.
    //
    // A `chain`ed empty tail rather than a `spawn`: the latter would require
    // handing over ownership of the stream or starting another task to watch for
    // its end, whereas all that is needed here is "do one more thing after the
    // last frame". A failed accounting write only logs (see `record_usage`) —
    // the conversation has already completed, and throwing its result away
    // because the ledger entry failed has the priorities backwards
    let st_for_usage = st.clone();
    let user_for_usage = user.clone();
    let tail = futures::stream::once(Box::pin(async move {
        let (input, output, model) = usage_seen
            .lock()
            .map(|g| (g.0, g.1, g.2.clone()))
            .unwrap_or_default();
        st_for_usage
            .record_usage(
                &user_for_usage,
                "llm",
                crate::quota::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    // Bring-your-own-key usage is still recorded but does not
                    // count against the quota — "how much did I spend this
                    // month" and "how much allowance do I have left" are two
                    // different questions
                    own_key: used_own_key,
                },
                &model,
            )
            .await;
        // This frame is not sent to the client: `filter_map` makes it disappear
        Option::<Result<Event, Infallible>>::None
    }));
    // Name futures' trait explicitly: tokio_stream's StreamExt is also in scope,
    // and its filter_map is synchronous (it takes an Option, not a Future) —
    // letting the compiler pick would select that one, and the resulting error
    // points at the async block
    let stream = futures::StreamExt::filter_map(
        futures::StreamExt::chain(stream, tail),
        |x| async move { x },
    );

    // keep-alive for the same reason as any other streaming endpoint: while the
    // model is "thinking" for a long time, intermediate proxies treat a
    // connection with no bytes flowing as dead and cut it
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

// ────────────────────────── memory retrieval ──────────────────

async fn memory_search(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MemorySearchQuery>,
) -> Result<Json<MemorySearchResponse>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.memory_search(&tenant, q).await?))
}

// ────────────────────────── episodes ───────────────────────────

/// The local agent writing one conversational turn back into the memory
/// database.
///
/// One request does all three of "write the episode + retrieve + attribute";
/// reasoning in [`cortex_proto::episodes`]'s module comment (splitting them
/// would put attribution in the client's hands).
///
/// **Idempotent**: writing the same id a second time returns 200 with
/// `already_existed = true`, not 409. Offline queue replay delivers duplicates,
/// and that is an expected, normal path — an error would force the client to
/// handle a normal outcome as an exception, and that code never runs day to day
/// and executes for the first time exactly when something has gone wrong.
async fn write_episode(
    State(st): State<AppState>,
    headers: HeaderMap,
    // The scope carried by a delegated credential, inserted by the
    // authentication middleware. **The origin is decided by the credential, not
    // by the request body** — this is the crux of the entire memory write path:
    // letting the client declare "I am low trust" is letting a prompt-injected
    // agent pick its own trust tier. `None` means this is a full user
    // credential, with no downgrade.
    delegation: Option<axum::Extension<crate::delegated_token::DelegatedScope>>,
    Json(req): Json<NewEpisodeRequest>,
) -> Result<Json<EpisodeAck>, ApiError> {
    // **Only the one carrying an anchor costs money** (it triggers extraction).
    // One without merely stores the raw text, and blocking that is pointless —
    // it is also the majority half during an import, so blocking it would turn
    // "out of allowance" into "the raw text cannot be stored either", losing
    // data for nothing
    if req.anchor_episode_id.is_some() {
        let user = crate::accounts::current_user(&st, &headers).await;
        st.enforce_quota(&user).await?;
    }
    let tenant = st.tenant(&headers).await?;
    // A delegated caller may only write to **its own session**. Without this, a
    // prompt-injected agent could write fabricated "things the user said" into
    // **any** of that person's sessions — and binding the scope to a session is
    // entirely about this
    if let Some(axum::Extension(scope)) = &delegation
        && scope.session_id != req.session_id
    {
        tracing::warn!(
            owner = %scope.owner, bound = %scope.session_id, attempted = %req.session_id,
            "a delegated caller tried to write to a different session"
        );
        return Err(ApiError::bad_request(
            "this credential may only write conversation records for the session it is bound to",
        ));
    }
    Ok(Json(
        st.write_episode(
            &tenant,
            req,
            // **The origin is decided by the credential** (see the paragraph
            // above). A delegated credential → tier 3; a full user credential →
            // tier 2. The tier for third parties writing in over MCP (tier 4)
            // does not use this route; it goes through `crate::mcp`'s remember
            // tool
            if delegation.is_some() {
                cortex_memory::extract::TurnOrigin::Delegated
            } else {
                cortex_memory::extract::TurnOrigin::Trusted
            },
        )
        .await?,
    ))
}

async fn get_episode(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<EpisodeDto>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.get_episode(&tenant, &id).await?))
}

// ─────────────────────────── /projects ─────────────────────────

/// The projects that exist. Deleted ones are not returned — deleting a project
/// means "disappear from the list".
async fn list_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ProjectsResponse>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(ProjectsResponse {
        projects: st.list_projects(&tenant).await?,
    }))
}

/// Create a project. The id is generated server-side and returned in the
/// response.
async fn create_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NewProjectRequest>,
) -> Result<Json<ProjectDto>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.create_project(&tenant, &req.name).await?))
}

/// Rename. Returns the project as it stands after the change.
async fn patch_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<ProjectPatch>,
) -> Result<Json<ProjectDto>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.patch_project(&tenant, &id, patch).await?))
}

/// Delete a project — **dissolve the grouping, leave the content alone**.
///
/// The sessions inside revert to ungrouped, with every message, attachment and
/// extracted memory intact. So this DELETE is nothing at all like `redact` /
/// `purge`, and the UI copy must not describe it as "permanently delete".
///
/// Returns an empty object rather than a 204: it keeps the client's JSON
/// decoding path uniform, with one fewer "this response has no body" special
/// case.
async fn delete_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    st.delete_project(&tenant, &id).await?;
    Ok(Json(serde_json::json!({})))
}

/// The session list. **Archived ones are not returned by default** — the
/// product semantics of archiving is precisely "disappear from the list". Pass
/// `?include_archived=true` to see everything, or `?project_id=` to see one
/// project.
async fn list_sessions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListSessionsQuery>,
) -> Result<Json<SessionsResponse>, ApiError> {
    Ok(Json(SessionsResponse {
        sessions: st.list_sessions(&st.tenant(&headers).await?, &q).await?,
    }))
}

/// One session's detail: the summary plus **one page** of messages.
///
/// The newest page by default; page backwards with `?before=` (taking
/// `next_cursor` from the previous response). An older client sending no
/// parameters receives the **newest** N — previously it received the oldest N,
/// i.e. a conversation with no ending, with no way to tell it had been
/// truncated.
async fn get_session(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<SessionDetailQuery>,
) -> Result<Json<SessionDetail>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.session_detail(&tenant, &id, &q).await?))
}

/// Rename / archive / bind a workspace. Returns the session summary as it stands
/// after the change.
///
/// Archiving goes through here rather than `DELETE /sessions/{id}`: that verb
/// would leave the client (and whoever reads the code) believing the data is
/// gone. In an append-only system not one row is lost; it simply stops appearing
/// in the default list — actually destroying content is redact / purge, a
/// different path requiring a second confirmation.
async fn patch_session(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<SessionPatch>,
) -> Result<Json<SessionDto>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.patch_session(&tenant, &id, patch).await?))
}

// ──────────────────────────── /blobs ───────────────────────────

/// Server-relayed upload. The request body is raw bytes, and `Content-Type` is
/// a **fallback** MIME declaration (whenever the byte header identifies the
/// type, the byte header wins).
async fn upload_blob(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<BlobDto>, ApiError> {
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        // Strip parameters such as `; charset=utf-8`: blobs.mime stores the bare
        // type
        .map(|v| v.split(';').next().unwrap_or(v).trim())
        .filter(|v| !v.is_empty());
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.upload_blob(&tenant, body, declared).await?))
}

async fn presign_blob(
    State(st): State<AppState>,
    Json(req): Json<BlobPresignRequest>,
) -> Result<Json<BlobPresignResponse>, ApiError> {
    ensure_presign_supported(&st)?;
    Ok(Json(st.presign_upload(&req.hash).await?))
}

/// The local fallback backend cannot sign URLs. **Check before issuing the
/// request**, and answer 501 rather than 500 — the former tells the client
/// "this deployment does not support direct upload, switch to relaying through
/// POST /blobs", while the latter leaves it treating this as a transient fault
/// and retrying forever down a road that will never open.
fn ensure_presign_supported(st: &AppState) -> Result<(), ApiError> {
    if st.supports_presign() {
        Ok(())
    } else {
        Err(ApiError::unsupported(format!(
            "the {} object-storage backend cannot sign presigned URLs; relay the upload \
             through POST /blobs and fetch it back with GET /blobs/{{hash}} instead",
            st.blob_backend()
        )))
    }
}

async fn commit_blob(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BlobCommitRequest>,
) -> Result<Json<BlobDto>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.commit_blob(&tenant, req).await?))
}

async fn get_blob_url(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Json<BlobUrlResponse>, ApiError> {
    ensure_presign_supported(&st)?;
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.blob_download_url(&tenant, &hash).await?))
}

/// Fetch the bytes, with `Range` support.
///
/// # Why relay rather than always 302 to a presigned URL
///
/// A redirect saves bandwidth, but it welds two requirements together: the
/// client must be able to reach object storage directly, and
/// `LocalFsBlobStore` cannot sign a URL at all. Relaying works on both backends
/// and on an intranet deployment, making it the floor that **always works**. A
/// client that wants to save bandwidth uses `GET /blobs/{hash}/url` to fetch a
/// presigned URL itself — an explicit choice, rather than the server guessing
/// on its behalf.
///
/// Range support is mandatory: without it, a player scrubbing to 31:40 has to
/// pull the entire video down first.
async fn get_blob(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let tenant = st.tenant(&headers).await?;
    let (mime, size) = st.blob_meta(&tenant, &hash).await?;
    let total = u64::try_from(size).unwrap_or(0);

    let raw_range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    match parse_range(raw_range.as_deref(), total) {
        RangeSpec::Full => {
            let bytes = st.blob_bytes(&hash, None).await?;
            Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    // Without this header a player never even attempts a
                    // Range — it dutifully pulls the whole file from the
                    // start, and scrubbing then becomes a disaster
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                ],
                bytes,
            )
                .into_response())
        }
        RangeSpec::Partial(r) => {
            // Content-Range's right end is **closed**, one off from Rust's
            // half-open interval
            let (start, end) = (r.start, r.end - 1);
            let bytes = st.blob_bytes(&hash, Some(r)).await?;
            Ok((
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    ),
                ],
                bytes,
            )
                .into_response())
        }
        RangeSpec::Unsatisfiable => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            // A 416 must carry `bytes */total` — that is how the client learns
            // which range it should have asked for; without it, all it can do is
            // blindly retry the same out-of-bounds range
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
        )
            .into_response()),
    }
}

// ──────────────────────────── /sync ────────────────────────────

async fn sync(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SyncQuery>,
) -> Result<Json<SyncResponse>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    Ok(Json(st.sync_since(&tenant, q).await?))
}

// ──────────────────────────── errors ──────────────────────────

#[derive(Debug)]
pub struct ApiError {
    inner: cortex_core::CortexError,
    /// Overrides `inner`'s wording.
    ///
    /// `CortexError`'s `Display` prefixes every variant ("storage error:" and
    /// the like), which is useful for internal logs and noise for the user —
    /// "wrong username or password" with "storage error:" in front of it only
    /// leaves people thinking the service is broken. When one of its variants is
    /// borrowed merely to carry a status code, the wording has to be supplied
    /// separately.
    message: Option<String>,
    /// Overrides the default mapping in
    /// [`cortex_core::CortexError::http_status`].
    ///
    /// Used only in the few places where **the domain error cannot express the
    /// distinction**. `CortexError` deliberately has no "this deployment does
    /// not support it" variant — that is a property of the deployment shape,
    /// not a domain concept, and polluting the global error type for it does not
    /// pay.
    status: Option<StatusCode>,
}

impl ApiError {
    /// 400: what the client sent is wrong.
    pub fn bad_request(message: impl Into<String>) -> Self {
        cortex_core::CortexError::Invalid(message.into()).into()
    }

    /// 401: no valid credential.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            inner: cortex_core::CortexError::Store("unauthorized".into()),
            status: Some(StatusCode::UNAUTHORIZED),
        }
    }

    /// 429: the allowance is used up.
    ///
    /// Kept apart from 403: 403 means "you may never do this", 429 means "not
    /// now, but you can later". The client uses that to decide whether to offer
    /// a retry.
    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            inner: cortex_core::CortexError::Store("quota".into()),
            status: Some(StatusCode::TOO_MANY_REQUESTS),
        }
    }

    /// 403: we recognise who you are, but this is not permitted.
    ///
    /// Kept apart from 401: 401 means "go log in", 403 means "logging in will
    /// not help". Closed registration answers 403 rather than 401, or the client
    /// pops up a login box that cannot help.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            inner: cortex_core::CortexError::Store("forbidden".into()),
            status: Some(StatusCode::FORBIDDEN),
        }
    }

    /// 500: something went wrong on our side.
    pub fn internal(message: impl Into<String>) -> Self {
        cortex_core::CortexError::Store(message.into()).into()
    }

    /// The sentence a human reads. The SSE path has to put it inside an event —
    /// there is no HTTP status code available there, only an `error` event.
    #[must_use]
    pub fn message(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| self.inner.to_string())
    }

    /// 501: nothing is wrong with the request; this deployment shape can
    /// **never** provide this capability.
    ///
    /// The client treats a 501 as "this road will not open", degrades the
    /// feature accordingly, and **does not retry** (`isUnsupported` in
    /// `api_exception.dart`, whose comment reads "retrying a 501 is a loop on a
    /// road that will never open"). So only a genuinely permanent absence
    /// deserves it.
    ///
    /// The boundary with [`Self::conflict`]: that one says "the capability
    /// exists, **this session** simply is not here", which the user can fix by
    /// doing something; this one says "this road will never open".
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            inner: cortex_core::CortexError::Store("unsupported".into()),
            status: Some(StatusCode::NOT_IMPLEMENTED),
        }
    }
}

impl From<cortex_core::CortexError> for ApiError {
    fn from(e: cortex_core::CortexError) -> Self {
        Self {
            inner: e,
            status: None,
            message: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let code = self.status.unwrap_or_else(|| {
            StatusCode::from_u16(self.inner.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        });
        tracing::warn!(error = %self.inner, status = code.as_u16(), "request failed");
        (
            code,
            Json(ErrorBody {
                error: self.message(),
            }),
        )
            .into_response()
    }
}

// ══════════════════════════════════════════════════════════════
//  "Which routes sit behind the authentication middleware"
// ══════════════════════════════════════════════════════════════
//
// This group of tests guards something that previously had **no protection
// whatsoever**. Two real failure paths:
//
// 1. Whoever adds a route slips and adds it to `public`
// 2. A `.route(...)` **after** `route_layer` — axum's `route_layer` only covers
//    routes registered before it, so anything after is bare, while compilation,
//    clippy and the existing tests are all green
//
// Two separate locks handle those two:
//
// - Routes registered inside the macro → `every_protected_route_rejects_
//   anonymous_requests` walks [`PROTECTED_ROUTES`] (same source as the
//   registration chain, so nothing can be missed) and issues requests against
//   the **real** `router()`. It simultaneously covers "somebody moved
//   route_layer into the middle of the macro expansion".
// - Routes registered **outside** the macro →
//   `router_registers_no_routes_outside_the_macro` reads this file's own source.
//   That is the only means of catching it: axum does not expose its route table,
//   so there is no way at runtime to enumerate "the path I do not know exists".
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;
    use crate::state::Runtime;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt as _;

    /// The probe value that parameter placeholders are replaced with.
    ///
    /// The probe path is derived from the pattern **automatically** rather than
    /// having somebody write concrete paths into the inventory a second time —
    /// that would become yet another hand-maintained artifact, and therefore
    /// another place to forget to update.
    const PROBE: &str = "cortex-route-guard";

    fn probe_path(pattern: &str) -> String {
        pattern
            .split('/')
            .map(|seg| {
                if seg.starts_with('{') && seg.ends_with('}') {
                    PROBE
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// A state with authentication enabled, plus the plaintext of that token.
    fn app_with_token() -> (Router, String) {
        let (token, digest_hex) = crate::auth::generate();
        let raw = hex::decode(&digest_hex).expect("a generated digest should be valid hex");
        let digest: [u8; 32] = raw.try_into().expect("SHA-256 should be 32 bytes");
        let rt = Runtime {
            auth: AuthMode::Token { digest },
            tickets: std::sync::Arc::new(crate::auth::TicketBook::default()),
            // These route tests do not touch the account endpoints, so no
            // database is attached. The account endpoints panic without one, and
            // that is precisely the signal that they should not have been routed
            // to
            accounts: None,
            access: std::sync::Arc::new(crate::accounts::AccessBook::default()),
            delegations: std::sync::Arc::new(crate::delegated_token::DelegatedTokens::default()),
            // The route tests do not touch docker. A request with
            // `sandbox: true` gets "this deployment has no cloud sandbox" here,
            // which is exactly what it should get
        };
        (router(crate::state::AppState::for_tests(rt)), token)
    }

    async fn status_of(
        app: &Router,
        method: &Method,
        path: &str,
        bearer: Option<&str>,
    ) -> StatusCode {
        let mut req = Request::builder().method(method.clone()).uri(path);
        if let Some(t) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        // Every write method carries a valid empty JSON body: without one the
        // request dies inside the **handler** on a parse failure. That does not
        // affect the 401 assertion (the middleware runs first), but it would
        // make the other half — "with a credential it is no longer 401" — far
        // less convincing
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("building the request must not fail");
        app.clone()
            .oneshot(req)
            .await
            .expect("the router's error type is Infallible")
            .status()
    }

    /// Every entry on the inventory must 401 without a credential.
    ///
    /// # Why the probe must first be proven to have reached a route
    ///
    /// `route_layer` applies only to routes that **matched**, so a mistyped
    /// probe path comes back 404 — and `404 != 401` makes "this route is not
    /// protected" and "this route does not exist at all" produce the same green.
    /// In other words, a test that only asserts 401 may be **vacuous**.
    ///
    /// The way to prove it is to hit the path with a method this route does
    /// **not** declare: a matched path gives 405, and only an unmatched one
    /// gives 404. That criterion is independent of what the handler returns for
    /// business reasons — `GET /sessions/{id}` legitimately returns 404 because
    /// "the mock has no such session", so using 404 as the criterion would
    /// misjudge it as a nonexistent route.
    #[tokio::test]
    async fn every_protected_route_rejects_anonymous_requests() {
        let (app, token) = app_with_token();
        assert!(
            !PROTECTED_ROUTES.is_empty(),
            "the inventory is empty — the macro generated nothing, so this group of tests did \
             not really run"
        );

        for (pattern, methods) in PROTECTED_ROUTES {
            let path = probe_path(pattern);

            // First prove this probe path genuinely matches a route, or the
            // assertions below are vacuous. At least one candidate must be a
            // method this route does not declare. There are three because
            // `/settings/llm-key` carries GET / PUT / DELETE on one path —
            // there used to be only two candidates, and adding that route made
            // this panic outright. That is not a bad thing: it forces somebody
            // to come and check that the probe still holds
            let unsupported = [Method::DELETE, Method::PUT, Method::PATCH]
                .into_iter()
                .find(|m| !methods.contains(m))
                .expect(
                    "DELETE / PUT / PATCH are all taken by some route — pick another method \
                     this route does not declare as the probe, or give it a test of its own",
                );
            let routed = status_of(&app, &unsupported, &path, Some(&token)).await;
            assert_eq!(
                routed,
                StatusCode::METHOD_NOT_ALLOWED,
                "probe path {path} (from {pattern}) hit with the undeclared {unsupported} \
                 returned {routed}, expected 405. A 405 means the path matched and only the \
                 method was wrong; a 404 means this probe never reached a route at all, in which \
                 case the 401 assertion below is vacuous"
            );

            for method in *methods {
                let anon = status_of(&app, method, &path, None).await;
                assert_eq!(
                    anon,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} returned {anon} without a credential instead of 401 — \
                     this route is not behind the authentication middleware. Check whether it was \
                     added to public, or added after route_layer"
                );

                let authed = status_of(&app, method, &path, Some(&token)).await;
                assert_ne!(
                    authed,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} still 401s with a correct credential — authentication \
                     itself is broken"
                );
            }
        }
    }

    /// A wrong credential is refused just like no credential.
    #[tokio::test]
    async fn a_wrong_token_is_rejected_on_every_protected_route() {
        let (app, _) = app_with_token();
        for (pattern, methods) in PROTECTED_ROUTES {
            let path = probe_path(pattern);
            for method in *methods {
                let got = status_of(&app, method, &path, Some("not-the-token")).await;
                assert_eq!(
                    got,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} accepted a wrong token (returned {got})"
                );
            }
        }
    }

    /// **cortexd runs no agent and orchestrates no containers.** These paths are
    /// simply not on it.
    ///
    /// # This invariant got stronger twice; do not revert it
    ///
    /// The first time: cortexd once contained an in-process agent loop
    /// (`chat_here` + `run_turn`) whose tool catalogue held only
    /// `memory_search`, reached only when docker was unavailable. It was a
    /// **second implementation** — the same `Turn::run` assembled in two
    /// places, so every added tool and every change to injection discipline had
    /// to be made twice, and the one that was missed made no test go red.
    /// After deleting it, this test asserted a 501 that said "docker required"
    /// out loud.
    ///
    /// The second time (now): container orchestration moved to `cortex-agentd`
    /// too, so `/chat` and `/sandbox/*` **do not exist on this process at all**.
    /// Changing the assertion from 501 to 404 makes it stronger, not weaker —
    /// 501 says "I have this road but do not support it", 404 says "this road is
    /// not mine", and the latter is the truth.
    ///
    /// Whoever deploys cortexd standalone (a Cormex user) gets exactly this: a
    /// memory service, with no agent, no docker, and no pretence of either.
    ///
    /// Adding any one of them back produces no symptom anywhere else: it
    /// compiles, every other test is green, and our own deployment keeps working
    /// thanks to edge routing. **This is the only test that goes red.**
    #[tokio::test]
    async fn cortexd_serves_no_agent_shaped_route() {
        let (app, token) = app_with_token();
        for (method, path) in [
            (Method::POST, "/chat"),
            (Method::GET, "/sandbox/files"),
            (Method::GET, "/sandbox/workspace.tar"),
            (Method::POST, "/sandbox/snapshots"),
        ] {
            let req = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .expect("building the request must not fail");
            let got = app
                .clone()
                .oneshot(req)
                .await
                .expect("the router's error type is Infallible")
                .status();
            assert_eq!(
                got,
                StatusCode::NOT_FOUND,
                "{method} {path} belongs to cortex-agentd — that process has docker, this one \
                 does not. Actually returned {got}"
            );
        }
    }

    /// `/health` is the **only** endpoint that needs no authentication.
    /// Reasoning in [`Health::auth`].
    #[tokio::test]
    async fn health_is_the_only_endpoint_open_to_anonymous_callers() {
        let (app, _) = app_with_token();
        let got = status_of(&app, &Method::GET, "/health", None).await;
        assert_eq!(
            got,
            StatusCode::OK,
            "/health must be unauthenticated — its consumers are container probes that cannot \
             be given a credential"
        );
    }

    /// A nonexistent path must 404 and must not be judged 401 by the middleware
    /// first.
    ///
    /// This is the entire point of `route_layer` (rather than `layer`):
    /// otherwise "the path was typed wrong" and "the credential is wrong" look
    /// identical to the client.
    #[tokio::test]
    async fn an_unknown_path_is_a_404_not_a_401() {
        let (app, _) = app_with_token();
        let got = status_of(&app, &Method::GET, "/no/such/endpoint", None).await;
        assert_eq!(
            got,
            StatusCode::NOT_FOUND,
            "an unknown path returned {got} — was route_layer replaced with layer?"
        );
    }

    /// Only the `/health` route may be registered inside `router()`'s body.
    ///
    /// # Why this can only be done by reading the source
    ///
    /// The tests above walk the macro-generated inventory, which makes them
    /// inherently blind to "a route registered outside the macro" — and that is
    /// exactly the most dangerous one: a `.route(...)` after `.route_layer()`
    /// leaves that route unauthenticated while compilation and clippy stay
    /// green. axum does not expose its registered route table, so there is no
    /// way at runtime to enumerate "the path I do not know exists".
    ///
    /// So this settles for the next best thing: look directly at the body of
    /// `pub fn router` in this file. It is short, its shape is fixed, and **any**
    /// new route seeking to bypass authentication has to appear here first.
    ///
    /// Cut a span out of this file's source and drop whole-line comments.
    ///
    /// A `.route(` appearing inside a comment would corrupt the check. No string
    /// literal in this file contains `//`, so a line-based test is enough — and
    /// if somebody does write `//` into a string, this test goes red rather than
    /// lax, which is the right direction.
    fn code_between(src: &str, from: &str, to: &str) -> String {
        // Normalise CRLF first. One checkout with `core.autocrlf=true` on
        // Windows is enough to make every search for `\n}\n` come up empty, at
        // which point these tests fail with "cannot find the end" — a safe
        // direction (red, not green), but pure noise
        let src = src.replace("\r\n", "\n");
        let start = src.find(from).unwrap_or_else(|| {
            panic!("cannot find {from:?} in the source — was it renamed? this test has to follow")
        });
        let rest = &src[start..];
        let end = rest
            .find(to)
            .unwrap_or_else(|| panic!("cannot find the end {to:?} starting from {from:?}"));
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn router_registers_no_routes_outside_the_macro() {
        const SRC: &str = include_str!("routes.rs");

        let code = code_between(SRC, "\npub fn router(", "\n}\n");

        let registrations = code.matches(".route(").count();
        assert_eq!(
            registrations, 1,
            "router()'s body contains {registrations} occurrences of `.route(`, and there should \
             be only the public_routes() loop.
             New routes belong in the protected_routes! macro:
             · adding to public_routes() = no authentication at all (every entry there must state \
             its reason);
             · adding after protected_router(...) = registered after route_layer, equally \
             unauthenticated and more insidious.
             The actual function body:
{code}"
        );
        assert!(
            code.contains("public_routes()"),
            "the single .route() in router() should come from the public_routes() inventory"
        );
    }

    /// **The unauthenticated inventory should hold only these.**
    ///
    /// It is the service's only unauthenticated entrance, and the cost of adding
    /// one entry is silent: that path is open to everyone from then on, with no
    /// runtime means of discovering it should not have been. This test forces
    /// "the inventory grew" to be confirmed explicitly, once.
    #[test]
    fn the_public_list_stays_short() {
        let paths: Vec<&str> = super::public_routes().into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            paths,
            [
                "/health",
                "/auth/login",
                "/auth/refresh",
                "/auth/logout",
                "/auth/register",
            ],
            "the unauthenticated inventory changed. Before changing it, go add a sentence to \
             public_routes()'s documentation saying \"why this one cannot require a \
             credential\" — if you cannot write one, it belongs in protected_routes!"
        );
    }

    /// In the macro expansion, `.route_layer()` must come after **every**
    /// `.route()`.
    ///
    /// axum's `route_layer` only covers routes registered before it. Move it
    /// into the middle and the routes after it are bare — [`PROTECTED_ROUTES`]
    /// still lists them, so `every_protected_route_rejects_anonymous_requests`
    /// catches that variant. But the variant where **a new `.route()` is written
    /// inside the macro, after route_layer, and not added to the inventory** is
    /// undetectable at runtime (it is not on the inventory, so nobody hits it).
    /// This test fills exactly that gap.
    #[test]
    fn the_auth_layer_is_applied_after_every_route_in_the_macro() {
        const SRC: &str = include_str!("routes.rs");

        let code = code_between(SRC, "macro_rules! protected_routes", "\n}\n");
        let last_route = code
            .rfind(".route(")
            .expect("the macro should contain .route() registrations — without any, the whole macro is empty");
        let layer = code.rfind(".route_layer(").expect(
            "the macro should contain .route_layer() — without it, no route is authenticated",
        );
        assert!(
            layer > last_route,
            "the protected_routes! macro has a .route() after .route_layer().\n\
             axum's route_layer only covers routes registered before it — the one after has no \
             authentication,\n\
             and compilation, clippy and the existing tests are all green. Move .route_layer() \
             back to the final statement."
        );
    }

    /// `/health` must not appear on the inventory.
    ///
    /// This guards the opposite direction: somebody "helpfully" moves /health
    /// into the macro too, every container probe 401s, and then somebody most
    /// likely deletes the HEALTHCHECK.
    #[test]
    fn health_is_not_in_the_protected_list() {
        assert!(
            !PROTECTED_ROUTES.iter().any(|(p, _)| *p == "/health"),
            "/health has to stay on the public side — its consumers cannot be given a credential"
        );
    }

    // ────── invalidated facts are visible in retrieval results ─────

    /// On the mock backend: ordinary retrieval returns no invalidated facts,
    /// while replay returns them carrying `invalidated`.
    ///
    /// This goes through the real routes rather than calling `AppState`
    /// directly: the consumer of this contract is the client's CI, and what it
    /// sees is the HTTP response body.
    #[tokio::test]
    async fn replay_shows_invalidated_facts_and_plain_search_does_not() {
        async fn facts(app: &Router, token: &str, query: &str) -> Vec<serde_json::Value> {
            let req = Request::builder()
                .method(Method::GET)
                .uri(query)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("building the request must not fail");
            let resp = app.clone().oneshot(req).await.expect("Infallible");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "the retrieval endpoint should return 200"
            );
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .expect("the response body should be readable");
            let v: serde_json::Value = serde_json::from_slice(&bytes).expect("should be JSON");
            v["facts"]
                .as_array()
                .expect("facts should be an array")
                .clone()
        }

        let (app, token) = app_with_token();

        let plain = facts(&app, &token, "/memory/search?q=").await;
        assert!(!plain.is_empty(), "mock retrieval should not be empty");
        assert!(
            plain.iter().all(|f| f["invalidated"] == false),
            "no invalidated fact may appear in ordinary retrieval — all four retrieval paths \
             query active_facts only"
        );

        let replayed = facts(&app, &token, "/memory/search?q=&as_of=2026-08-31T00:00:00Z").await;
        assert!(
            replayed.iter().any(|f| f["invalidated"] == true),
            "replay must return facts that were valid then and have since been overturned, and \
             mark them as such — \"what did I believe three months ago\" is exactly this \
             project's selling point, and filtering it out amounts to cutting that point away"
        );
        assert!(
            replayed.iter().all(|f| f.get("invalidated").is_some()),
            "every fact must carry the invalidated field; the client should not have to guess"
        );
    }
}
