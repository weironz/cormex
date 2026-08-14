//! Delegated credentials — the short-lived keys the memory service hands to
//! **untrusted callers**.
//!
//! This is not sandbox infrastructure, even though the sandbox is its only
//! caller so far. The question answered here is "**what can something I do not
//! trust do to me**" — which is the memory service's own inbound authorization
//! policy, independent of who is running containers out there. Issuing goes
//! through `POST /delegated-tokens`, carrying **the caller's own bearer**: with
//! no service credential involved, there is no privilege-escalation surface.
//!
//! The reasoning below uses the sandbox as its example, because that is the
//! scenario that has actually been validated. Every point holds equally for any
//! other low-trust caller (a third-party agent, a CI job, a throwaway script).
//!
//! # Why the user's login token cannot simply be handed over
//!
//! What runs on that side is **untrusted code**: the model's output, the
//! dependencies the user installed, prompt-injected tool calls — all of it.
//! Holding the user's bearer token means holding that person's entire memory,
//! every session, every attachment.
//!
//! OpenHands takes the other route here (shipping the LLM api key into the
//! container so the container talks to the provider itself). We do not — the
//! key stays on the memory service side and the caller proxies through
//! `/llm/stream`, which incidentally gives quota and billing a natural home.
//!
//! # This credential's scope is **more than a route allow-list**
//!
//! This is the least obvious part of the design. All four cloud agents
//! (Codex / Claude web / Devin / Manus) can get away with route-level isolation,
//! because **their sandboxes have no writable long-term memory**: all the
//! sandbox can touch is a cloned git working copy, and if it breaks you throw it
//! away and start over.
//!
//! We are different. The caller has to call `POST /episodes` (write the
//! conversation) and `GET /memory/search` (query memory), and those two are
//! **persistent channels that pierce that boundary**:
//!
//! - If `/episodes`'s `role` were freely chosen by the request body, a
//!   prompt-injected agent could impersonate "something the user said with their
//!   own mouth" straight into the extraction pipeline — and that fact would be
//!   retrieved and injected into the system prompt across **every future
//!   session, on every device**. **The container was destroyed long ago; the
//!   memory is still there.** This point is unique to a memory service: when the
//!   process dies and the container is deleted, what it wrote does not go with
//!   it.
//! - If `/memory/search` retrieved across the whole tenant, the caller could
//!   dredge up sensitive memories from that user's **other sessions** and ship
//!   them out through an egress channel.
//!
//! So "the blast radius is already bounded by the container" holds for those
//! four and does not hold for us. The scope has to descend to the **semantic**
//! level: see [`DelegatedScope`].
//!
//! # The credential is bound to a session, not merely to an owner
//!
//! One user may have several sessions. The caller should only be able to see its
//! own — otherwise "isolation" stops at the user level, while two pieces of
//! work belonging to the same user need a boundary just as much (one handling
//! company contracts, the other running a script copied off the internet).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Lifetime of a sandbox token.
///
/// Far longer than a ticket (60 seconds): this one is for long-lived
/// connections, and a container may live for hours. Still bounded, though — a
/// token should expire naturally once its container has been reaped, rather than
/// sitting in the table forever. Every [`DelegatedTokens::touch`] renews it, so
/// an active sandbox is never killed by mistake.
const TTL: Duration = Duration::from_secs(6 * 3600);

/// What the sandbox may do. **Authorized semantically, not by route.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedScope {
    /// Who the token belongs to. Tenant isolation is still decided by this —
    /// that layer already existed.
    pub owner: String,
    /// The session it is bound to. The sandbox may only read and write things
    /// belonging to this one session.
    pub session_id: String,
    /// Which project this session belongs to. `None` = ungrouped.
    ///
    /// **It determines the container and the volume** (see [`Self::key`]) and
    /// plays no part in [`Self::allows`] — what the token may do still depends
    /// on the session alone.
    pub project: Option<String>,
}

impl DelegatedScope {
    /// Containers and volumes are partitioned by this key.
    ///
    /// # Why by project rather than by user
    ///
    /// In the previous version each user had exactly one `/workspace`. So the
    /// files of two pieces of work — "the client contract" and "a script copied
    /// off the internet" — sat mixed together in one directory: dependency
    /// installs trampling each other, `ls` filling the screen with another
    /// project's things, while the user themselves had already separated the two
    /// (they created two projects).
    ///
    /// Partitioned by project, switching projects rebuilds the container (a
    /// 913 ms cold start, imperceptible to the user), and several sessions in the
    /// same project share one container.
    ///
    /// # Why ungrouped still uses the bare owner
    ///
    /// The identity mapping means **no data migration in production**. The
    /// existing volumes are named `cortex-ws-<owner>`, and none of their sessions
    /// have a project yet, so they keep matching as before. A new volume is only
    /// obtained once a session is moved into a project — and at that moment the
    /// user is explicitly "creating a new project", so an empty workspace is what
    /// they expect.
    ///
    /// # Why the project segment is a hash rather than the id itself
    ///
    /// **The container name is used as a DNS name** (when cortexd and the
    /// sandbox share a network, it connects directly to `http://<container
    /// name>`), and the hard limit on a DNS label is **63 bytes**. Two ULIDs
    /// concatenated directly:
    ///
    /// ```text
    /// cortex-sbx- + 26 + --p- + 26 = 67  ✗
    /// ```
    ///
    /// This was hit on a real machine, and the symptom is thoroughly misleading:
    /// the container is `Up (healthy)` and the agent inside logs "ready", while
    /// cortexd reports "no response for 30 seconds" — because it cannot even
    /// resolve the name.
    ///
    /// With 12 hex characters (48 bits) it comes to 11+26+4+12 = **53**, leaving
    /// ample headroom. The collision probability is negligible at the scale of
    /// "how many projects one user has".
    ///
    /// The price is that the project id cannot be read back out of the container
    /// name. The way to look it up is cortexd's per-turn log line
    /// (`本轮走云沙箱 owner=… project=… sandbox=…`) — it prints all three on one
    /// line. **Do not add a project parameter to the runner**: that trait taking
    /// only a scope and a spec hash is deliberate (a test guards "the caller
    /// cannot slip in a second mount"), and loosening it for the sake of one log
    /// line does not pay.
    ///
    /// # Why the separator needs no injection defence
    ///
    /// The owner is resolved server-side from the credential (`current_user`)
    /// and always comes first; the project is supplied by the client, but it can
    /// only alter the second half **under its own name** — the assembled key
    /// begins with its own owner no matter what, so it cannot reach anyone
    /// else's key. The hash incidentally also pins down how long an id a client
    /// can push in.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.project {
            None => self.owner.clone(),
            Some(p) => {
                use sha2::{Digest as _, Sha256};
                let digest = Sha256::digest(p.as_bytes());
                format!("{}--p-{}", self.owner, hex::encode(&digest[..6]))
            }
        }
    }

    /// Is this path + method allowed?
    ///
    /// # Why an allow-list rather than a deny-list
    ///
    /// Same argument as `WORKSPACE_FREE_TOOLS`: a deny-list missing a new route
    /// = the sandbox quietly gains a capability (silent, dangerous); an
    /// allow-list missing one = the sandbox lacks a capability (loud,
    /// immediately visible). **The failure directions differ, so choose the one
    /// that tips towards safety.**
    ///
    /// The list is the **actual calls** found by auditing the code, not
    /// guesswork. The consequence of omitting one has already been quantified:
    /// leave out `GET /sessions/{id}` and `load_history` silently degrades to an
    /// empty history, so a cloud session **loses its memory every turn with no
    /// error whatsoever**.
    #[must_use]
    pub fn allows(&self, method: &axum::http::Method, path: &str) -> bool {
        use axum::http::Method;
        match (method, path) {
            // Once or several times per turn. Both the cost and the benefit of
            // keeping the LLM key out of the container ride on this one
            (&Method::POST, "/llm/stream") => true,
            // Writing user / assistant episodes, plus outbox replay
            (&Method::POST, "/episodes") => true,
            // The memory_search tool
            (&Method::GET, "/memory/search") => true,
            // Pulling session history each turn. **Omitting this = amnesia every
            // turn, silently**
            (&Method::GET, p) if is_session_path(p) => true,
            // Asking "who does this credential belong to" at startup. Without
            // it, the state directory stays in _pending forever
            (&Method::GET, "/auth/me") => true,
            _ => false,
        }
    }
}

/// The `/sessions/{id}` shape (excluding `/sessions` itself — that lists every
/// session).
fn is_session_path(path: &str) -> bool {
    match path.strip_prefix("/sessions/") {
        // Exactly one level: `/sessions/abc` matches, `/sessions/abc/xxx` does
        // not. One more level is a different endpoint, and this list was audited
        // entry by entry
        Some(rest) => !rest.is_empty() && !rest.contains('/'),
        None => false,
    }
}

/// The sandbox tokens that have been issued.
///
/// Same shape as `TicketBook` (in memory, living and dying with the process).
/// After a cortexd restart every sandbox token is void, and that is **correct**:
/// those containers need to be taken over again anyway, and letting them keep
/// running with a still-valid credential is the actual problem.
#[derive(Default)]
pub struct DelegatedTokens {
    inner: Mutex<HashMap<String, (DelegatedScope, Instant)>>,
}

impl DelegatedTokens {
    /// Issue one. The caller must **already** have confirmed the owner's
    /// identity.
    pub fn issue(&self, scope: DelegatedScope) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect(
            "the kernel entropy source is unavailable; refusing to issue a predictable token",
        );
        let token = hex::encode(buf);
        let mut guard = self
            .inner
            .lock()
            .expect("the token table's lock should never be poisoned");
        // Prune the expired ones while we are here. As with TicketBook: issuing
        // is the only action that grows this map, so pruning here makes "grows
        // and never shrinks" impossible
        let now = Instant::now();
        guard.retain(|_, (_, at)| now.duration_since(*at) < TTL);
        guard.insert(token.clone(), (scope, now));
        token
    }

    /// Do we recognise this token, and if so what is its scope? Renews it along
    /// the way.
    #[must_use]
    pub fn resolve(&self, token: &str) -> Option<DelegatedScope> {
        let mut guard = self.inner.lock().ok()?;
        let now = Instant::now();
        let (scope, at) = guard.get_mut(token)?;
        if now.duration_since(*at) >= TTL {
            guard.remove(token);
            return None;
        }
        *at = now;
        Some(scope.clone())
    }

    /// The still-valid token held by this scope (user + project), if any.
    ///
    /// # Why reuse is needed instead of issuing a fresh one each turn
    ///
    /// The container's **inbound** authentication uses the token in its startup
    /// env (`cortex-local` shares one token between inbound and outbound,
    /// deliberately avoiding a second secret). Rotate it each turn and the
    /// second turn's proxied request is a 401 — and that error reads like "the
    /// sandbox is broken".
    ///
    /// The real-machine symptom "first turn works, second turn 401s" was exactly
    /// this. **A token's lifetime follows the container**, not the turn.
    ///
    /// Looked up by [`DelegatedScope::key`] rather than by owner: one user can
    /// have sandboxes for several projects alive at once, and picking the wrong
    /// one means **handing project A's token to project B's container** — the
    /// container then comes up and answers normally, it simply recognises a
    /// different token, and only the second turn 401s.
    #[must_use]
    pub fn find_by_key(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        let now = Instant::now();
        guard.iter().find_map(|(tok, (scope, at))| {
            (scope.key() == key && now.duration_since(*at) < TTL).then(|| tok.clone())
        })
    }

    /// Rebind an existing token to a different session.
    ///
    /// Used when the same container serves this user's next session. **This does
    /// not widen the scope** — afterwards it still matches exactly one session,
    /// just a different one.
    pub fn rebind(&self, token: &str, session_id: &str) {
        if let Ok(mut g) = self.inner.lock()
            && let Some((scope, at)) = g.get_mut(token)
        {
            scope.session_id = session_id.to_owned();
            *at = Instant::now();
        }
    }

    /// Revoke (called when the sandbox stops).
    pub fn revoke(&self, token: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(token);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn scope() -> DelegatedScope {
        DelegatedScope {
            owner: "u1".into(),
            session_id: "s1".into(),
            project: None,
        }
    }

    fn in_project(owner: &str, project: &str) -> DelegatedScope {
        DelegatedScope {
            owner: owner.into(),
            session_id: "s1".into(),
            project: Some(project.into()),
        }
    }

    /// The allow-list is **exactly** the five audited entries and nothing more.
    ///
    /// Each one has a price: one extra entry is one more capability the sandbox
    /// has for piercing the container boundary.
    ///
    /// `/confirmations` never made it onto this list, and by now even its
    /// **existence** no longer holds: cortexd does not run an agent, so it has
    /// no such ledger (see the comment on the route list in `routes.rs`). This
    /// negative case therefore shifts from "prevent an unbounded recursion" to
    /// "prevent someone treating an endpoint that no longer exists as if it
    /// still did" — both of which should make it go red.
    #[test]
    fn the_allowlist_is_exactly_what_the_agent_actually_calls() {
        let s = scope();
        for (m, p) in [
            (Method::POST, "/llm/stream"),
            (Method::POST, "/episodes"),
            (Method::GET, "/memory/search"),
            (Method::GET, "/sessions/01ABC"),
            (Method::GET, "/auth/me"),
        ] {
            assert!(
                s.allows(&m, p),
                "{m} {p} is called every turn; omitting it makes the whole sandbox unusable"
            );
        }

        for (m, p) in [
            // The confirmation loop: adding it back would resurrect that cycle
            (Method::POST, "/confirmations"),
            (Method::GET, "/confirmations"),
            // Listing every session — the sandbox should only see its own
            (Method::GET, "/sessions"),
            // Modifying a session (including changing its workspace) is not the
            // sandbox's decision to make
            (Method::PATCH, "/sessions/01ABC"),
            // Projects, attachments, sync: none of these are needed by the agent
            // loop
            (Method::GET, "/projects"),
            (Method::POST, "/blobs"),
            (Method::GET, "/sync"),
            (Method::POST, "/auth/ticket"),
            // Writing an episode is a POST; the GET is replaying somebody's
            // conversation
            (Method::GET, "/episodes/01ABC"),
            // One more path level is a different endpoint
            (Method::GET, "/sessions/01ABC/messages"),
        ] {
            assert!(
                !s.allows(&m, p),
                "{m} {p} is not on the audited call list, and admitting it hands the sandbox a \
                 capability it does not need"
            );
        }
    }

    /// **A compromised container cannot reach its own backups.**
    ///
    /// This stands on its own because what it guards is not a matter of degree
    /// ("grant slightly less"), but the **entire premise of the first layer of
    /// data protection**: snapshots are driven by the host and written where the
    /// container cannot reach, so an attacker with arbitrary command execution
    /// inside the container cannot delete them.
    ///
    /// That property comes entirely from the **topology** here (those routes are
    /// absent from the token allow-list), not from any runtime check. The day
    /// someone casually adds `/sandbox/snapshots` to `allows`, that whole layer
    /// of protection goes with it — and **there will be no symptom at all**,
    /// until the day somebody actually needs to restore.
    #[test]
    fn the_sandbox_cannot_reach_its_own_backups() {
        let s = scope();
        for (m, p) in [
            (Method::GET, "/sandbox/snapshots"),
            (Method::POST, "/sandbox/snapshots"),
            (Method::POST, "/sandbox/snapshots/01ABC/restore"),
        ] {
            assert!(
                !s.allows(&m, p),
                "{m} {p} was admitted — the first layer of data protection is void as of now: a \
                 compromised container can list, overwrite and (once purge lands) delete all of \
                 its own backups, and this has no symptom whatsoever"
            );
        }
    }

    #[test]
    fn a_token_resolves_to_its_scope_and_an_unknown_one_does_not() {
        let book = DelegatedTokens::default();
        let t = book.issue(scope());
        assert_eq!(book.resolve(&t).as_ref(), Some(&scope()));
        assert!(
            book.resolve("deadbeef").is_none(),
            "a token that was never issued must not be recognised"
        );
    }

    #[test]
    fn revoking_takes_effect_immediately() {
        let book = DelegatedTokens::default();
        let t = book.issue(scope());
        book.revoke(&t);
        assert!(
            book.resolve(&t).is_none(),
            "once the sandbox stops, its token must be void on the spot — the container may \
             still be alive (the last instant before stop), and it must no longer be writing \
             memory"
        );
    }

    /// An ungrouped key is **identically the owner** — this is what saves the
    /// production volumes from needing a migration.
    #[test]
    fn an_ungrouped_scope_key_is_just_the_bare_owner() {
        assert_eq!(
            scope().key(),
            "u1",
            "changing this instantly orphans every user's cortex-ws-<owner> volume in \
             production: the container comes up with an empty volume mounted, and what the user \
             sees is \"all my files are gone\""
        );
        assert_ne!(
            in_project("u1", "p1").key(),
            scope().key(),
            "a sandbox inside a project must be a different volume, or this whole change did \
             nothing"
        );
        assert_ne!(
            in_project("u1", "p1").key(),
            in_project("u1", "p2").key(),
            "two projects must be separated from each other as well — that is the thing users \
             actually complained about"
        );
    }

    /// The container name is used as a **DNS name**, so it must be ≤ 63 bytes.
    ///
    /// This test was written after the fact: the first version concatenated two
    /// ULIDs directly, producing a 67-byte container name. The symptom on a real
    /// machine is deeply misleading — the container is `Up (healthy)` and the
    /// agent inside logs "local agent ready", while cortexd reports "no response
    /// for 30 seconds, it may have crashed". What is actually happening is that
    /// `getent hosts <container name>` fails to resolve outright.
    ///
    /// The limit is hard-coded as 63 rather than referencing some constant: this
    /// is the **protocol** limit on a DNS label (RFC 1035), not a number we
    /// chose.
    #[test]
    fn a_container_name_built_from_a_scope_key_stays_within_the_dns_label_limit() {
        // The longest realistic input: two 26-character ULIDs
        let worst = DelegatedScope {
            owner: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            session_id: "s".into(),
            project: Some("01BX5ZZKBKACTAV9WEVGEMMVRZ".into()),
        };
        // `cortex-sbx-` is the prefix the orchestration side (a different
        // repository) uses to name containers. **It is a hard-coded constant
        // here, not imported from there** — the two repositories share only
        // HTTP, no library. So what this assertion guards against is "they
        // lengthened the prefix and this side does not know": the real explosion
        // happens over there, but it goes red here first
        let name_len = "cortex-sbx-".len() + worst.key().len();
        assert!(
            name_len <= 63,
            "the container name is {name_len} bytes, over the DNS label limit of 63. The symptom \
             is not \"creating the container failed\" — the container comes up, the health check \
             passes, and cortexd cannot even resolve its name; what gets reported is \"no \
             response for 30 seconds\""
        );
    }

    /// The same project id must compute **the same** key every time.
    #[test]
    fn the_project_hash_is_deterministic() {
        assert_eq!(
            in_project("u1", "p1").key(),
            in_project("u1", "p1").key(),
            "if the key were unstable, every conversational turn would go looking for a \
             container that does not exist — so every turn creates a new one, while the old ones \
             sit where they were with the files still in them"
        );
    }

    /// The project id comes from the client, but it **cannot alter anyone else's
    /// key**.
    #[test]
    fn a_client_supplied_project_id_cannot_reach_another_users_sandbox() {
        // A project id trying to collide into u2's namespace
        let evil = in_project("u1", "--p-x/../u2");
        assert!(
            evil.key().starts_with("u1"),
            "the key necessarily begins with the owner resolved server-side (current_user, never \
             from the request body), so whatever shape the project id takes it can only alter the \
             segment under its own name"
        );
    }

    #[test]
    fn expired_tokens_are_swept_when_a_new_one_is_issued() {
        let book = DelegatedTokens::default();
        {
            let mut g = book.inner.lock().unwrap();
            g.insert(
                "old".into(),
                (scope(), Instant::now() - TTL - Duration::from_secs(1)),
            );
        }
        assert_eq!(book.len(), 1);
        let fresh = book.issue(scope());
        assert_eq!(
            book.len(),
            1,
            "expired tokens should be swept when a new one is issued"
        );
        assert!(book.resolve(&fresh).is_some());
    }
}
