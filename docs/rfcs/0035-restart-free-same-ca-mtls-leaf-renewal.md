# RFC 0035: Restart-free same-CA mTLS leaf renewal

**Status:** Accepted | **Milestone:** v0.30 | **Date:** 2026-08-12

**Depends on:** RFC 0028 distributed service trust, RFC 0029 mutual-TLS trust
distribution, RFC 0032 signed service-trust validity, and RFC 0034
restart-free service-signing handoff.

## What this RFC decides

RFC 0035 decides how the running trust distributor and its three running
control clients replace their X.509 leaf certificates and private keys without
restarting and without changing either configured certificate authority.

The watched object is one complete, generation-numbered TLS identity bundle.
It contains the leaf chain, matching private key, and the public issuer CA used
to validate that identity. Generation 1 pins the issuer CA for the lifetime of
the process. A later candidate must validate for the same purpose under the
same pinned CA before an exact higher generation can become current.

This is **leaf renewal**, not CA migration. The distributor's configured
client-verification CA and every control's configured server-verification CA
remain static. Ed25519 policy and receipt signatures remain the application
authority.

## Summary

Before v0.30, the trust-distribution hop supported TLS 1.3 mutual
authentication, but every process built its TLS identity once at startup.
Replacing a leaf therefore required replacing the distributor or control
process and also discarded its unrelated runtime state.

v0.30 adds an optional watched-bundle mode beside the legacy static PEM-path
mode. The distributor reloads an already-validated `rustls::ServerConfig` for
future handshakes. A control builds a completely new `reqwest::Client` and
atomically publishes that client for future snapshot fetches and receipt
posts. Rebuilding the client is required: changing certificate bytes while
retaining an old connection pool could let nominally new work continue on an
old TLS session.

An established server connection remains on the certificate used by its
handshake. An already-started control operation retains its captured client
and may finish on its old connection. The first operation that snapshots the
new client cannot enter the old pool. The proof reports these semantics
truthfully; it does not claim that a leaf change renegotiates existing TLS 1.3
connections.

## Goals

- Load and validate the complete initial identity before binding a listener or
  accepting a remote trust snapshot.
- Require a bounded regular, non-symlink identity bundle with exact mode
  `0600` on Unix.
- Bind every bundle to the exact control cluster, identity ID, TLS purpose,
  and, for a server, configured DNS name.
- Require a positive generation, a usable certificate chain, exactly one
  supported matching private key, and a bounded issuer-CA set.
- Validate the leaf at observation time for chain, validity window, required
  server/client EKU, and server DNS SAN where applicable.
- Pin the generation-1 issuer CA semantics for the process lifetime. Reject a
  higher bundle under another CA even if that bundle is otherwise valid.
- Treat an identical current generation as unchanged, a semantically different
  current generation as a fork, and a lower generation as rollback.
- Build the complete replacement runtime object before publication and retain
  the last known good identity after every source, validation, ordering, or
  runtime-construction failure.
- Deduplicate stable deterministic failures, retry transient source failures,
  clear truthful error status after recovery, and supervise every watcher.
- Make new distributor handshakes use the new server leaf while established
  connections retain the old handshake identity until they close.
- Make each control fetch or receipt operation capture one complete HTTP client;
  after activation, new operations use a fresh connection pool and the new
  client leaf.
- Preserve snapshot publication, ETag reads, durable cache/floor behavior,
  trust-policy activation, convergence receipts, quorum, and real JSON/SSE
  service throughout the handoff.
- Retain the legacy static TLS path configuration when watched mode is absent.

## Non-goals

- CA migration, overlapping old/new trust roots, cross-signing, or changing a
  configured server/client verification CA.
- CRLs, OCSP, ACME, automated issuance, automated renewal scheduling, or an
  emergency revocation/cancellation authority.
- TLS 1.3 post-handshake certificate renegotiation or forced termination of
  established connections.
- Binding an X.509 subject or SAN to an InferLab Ed25519 service credential.
  Bundle metadata is process configuration; application authorization still
  comes from signed policy and receipt bytes.
- TLS expansion to Raft, gateway/control, gateway/worker, public, operator, or
  metrics listeners.
- HSM/KMS/TPM custody, encrypted private keys, memory zeroization, or proof that
  the old key disappears immediately from process memory.
- Fleet-atomic renewal. The distributor and controls change independently.
- Trust-distributor HA or a zero-failure guarantee if a certificate is allowed
  to expire before a valid successor is activated.

## Terms

| Term | Meaning here |
|---|---|
| Leaf | End-entity X.509 certificate plus its matching private key |
| Issuer CA | Public trust anchor(s) embedded in the bundle and pinned from generation 1 |
| Verification CA | Static CA configured for authenticating the remote side of mTLS |
| Identity bundle | Whole local JSON object containing metadata, leaf chain, private key, and issuer CA |
| Purpose | Exactly `server` or `client`; determines EKU and hostname validation |
| LKG | Last-known-good runtime identity retained after a rejected reload |
| Operation snapshot | One complete control HTTP client captured for a fetch or receipt post |
| Established connection | TLS connection whose handshake completed before activation |

## Identity bundle contract

The JSON schema is `inferlab.tls-identity-bundle.v1`:

```json
{
  "schema": "inferlab.tls-identity-bundle.v1",
  "cluster_id": "inferlab-primary",
  "generation": 2,
  "identity_id": "node-a",
  "purpose": "client",
  "certificate_chain_pem": "-----BEGIN CERTIFICATE-----\n...\n",
  "private_key_pem": "-----BEGIN PRIVATE KEY-----\n...\n",
  "issuer_ca_pem": "-----BEGIN CERTIFICATE-----\n...\n"
}
```

A server bundle also carries the exact configured name:

```json
{
  "identity_id": "trust-distributor",
  "purpose": "server",
  "server_name": "localhost"
}
```

Client bundles must omit `server_name`. Unknown fields fail. The total JSON
file is capped at 512 KiB; each embedded PEM component remains capped at 256
KiB; certificate-chain and CA counts remain capped at 32 each. The active
certificate is the first certificate in the chain.

The decoded semantic identity consists of cluster, generation, identity ID,
purpose, server name, ordered leaf/intermediate DER chain, private-key DER, and
the canonically ordered issuer-CA DER set. JSON whitespace, PEM whitespace,
and issuer-CA ordering alone do not create a fork. A changed certificate, key,
purpose, name, or CA at the same generation does.

The bundle contains private-key material. On Unix its path must resolve to the
same regular, non-symlink file that was inspected and opened, with exact mode
`0600`. The loader checks source/opened-file identity around the read so an
atomic replacement race is retried rather than trusted. Errors and debug/status
output never include the path, PEM, subject, serial number, or private key.

## Configuration contract

### Distributor server

Legacy static mode remains the RFC 0029 three-path group. Watched mode uses:

| Environment variable | Meaning |
|---|---|
| `INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_PATH` | Complete server identity bundle |
| `INFERLAB_TRUST_DISTRIBUTOR_TLS_IDENTITY_BUNDLE_POLL_MS` | Optional `25..=60000` ms interval; default `100` |
| `INFERLAB_TRUST_DISTRIBUTOR_TLS_SERVER_NAME` | Exact DNS name required in bundle and leaf SAN |
| `INFERLAB_TRUST_DISTRIBUTOR_TLS_CLIENT_CA_PATH` | Existing static CA for authenticating clients |

`identity_id` is fixed to `trust-distributor`; `cluster_id` comes from the
existing distributor configuration. Watched mode is mutually exclusive with
`INFERLAB_TRUST_DISTRIBUTOR_TLS_CERT_PATH` and
`INFERLAB_TRUST_DISTRIBUTOR_TLS_KEY_PATH`. Poll/name variables without a bundle
fail closed. Plain HTTP remains available only when all TLS variables are
absent.

### Control clients

The existing `INFERLAB_SERVICE_TRUST_TLS_CA_CERT_PATH` remains the static CA
used to authenticate the distributor. A watched client replaces the legacy
client certificate/key pair with:

| Environment variable | Meaning |
|---|---|
| `INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_PATH` | Complete client identity bundle |
| `INFERLAB_SERVICE_TRUST_TLS_CLIENT_IDENTITY_BUNDLE_POLL_MS` | Optional `25..=60000` ms interval; default `100` |

The expected bundle identity is the control's stable local service ID and the
expected cluster is the Raft/service-trust cluster. Watched mode is mutually
exclusive with `INFERLAB_SERVICE_TRUST_TLS_CLIENT_CERT_PATH` and
`INFERLAB_SERVICE_TRUST_TLS_CLIENT_KEY_PATH`. HTTPS still requires the static
server CA and exactly one client identity source. HTTP accepts neither source.

Every security-sensitive optional variable distinguishes absent, empty,
non-Unicode, partial, and mixed configuration and fails before service.

## Validation and activation state machine

```mermaid
stateDiagram-v2
    [*] --> StartupLoad
    StartupLoad --> ServingA: "valid g1 / leaf A / issuer CA pinned"
    StartupLoad --> FailedStartup: "source, binding, chain, time, EKU, SAN, or key failure"
    ServingA --> ServingA: "same g1 semantics / unchanged"
    ServingA --> ServingA: "invalid, expired, wrong-host, wrong-EKU, rollback, or fork / reject + LKG"
    ServingA --> ServingA: "valid g2 under another CA / reject + LKG"
    ServingA --> ServingB: "exact higher g2 under pinned CA / build then publish"
    ServingB --> ServingB: "same g2 semantics / unchanged + clear prior error"
    ServingB --> ServingB: "invalid g3 / reject + retain B"
    FailedStartup --> [*]
```

Validation order is:

1. bounded, safe file read and strict JSON/schema decoding;
2. exact cluster/identity/purpose/name binding;
3. bounded strict PEM decoding and certificate/private-key match;
4. current-time chain, EKU, and server-name verification against embedded CA;
5. generation and same-generation semantic comparison;
6. exact equality with the generation-1 pinned issuer-CA set;
7. complete runtime server-config or client/pool construction; and
8. publication plus state/counter update.

No failed step changes the current runtime object. The generation floor and CA
pin are in memory and reset at restart; startup revalidates the then-current
bundle but v0.30 does not claim durable TLS anti-rollback.

## Connection and operation semantics

```mermaid
sequenceDiagram
    participant Old as "established A connection"
    participant W as "identity watcher"
    participant Runtime as "server config or client snapshot"
    participant New as "new operation / handshake"

    Old->>Runtime: "handshake or operation captured A"
    W->>W: "validate g2/B under pinned CA"
    W->>Runtime: "publish complete B runtime"
    Old-->>Old: "may finish using A"
    New->>Runtime: "capture after publication"
    Runtime-->>New: "B and a fresh client pool"
```

`rustls` has no TLS 1.3 renegotiation. Reloading the distributor config changes
future handshakes only. On controls, a `reqwest::Client` owns its pool and
identity; therefore the whole client is the snapshot boundary. Request code
must obtain the current client once and use that clone through response/body
completion. It must not fetch the identity and pool through separate mutable
lookups.

## Failure matrix

| Candidate or event | Startup | Live reload | Runtime effect |
|---|---|---|---|
| Missing/unreadable/open-race source | Fail | Retry; bounded report | LKG retained |
| Symlink, non-regular, or non-`0600` file | Fail | Reject/deduplicate | LKG retained |
| Oversized/malformed/unknown-field bundle | Fail | Reject/deduplicate | LKG retained |
| Wrong cluster, identity, purpose, or server name | Fail | Reject/deduplicate | LKG retained |
| Malformed chain/key/CA or mismatched private key | Fail | Reject/deduplicate | LKG retained |
| Expired/not-yet-valid/wrong-EKU/wrong-SAN leaf | Fail | Reject/deduplicate | LKG retained |
| Leaf chaining to a different CA | Fail if invalid | Reject as CA change | LKG retained |
| Lower generation | Fail only if malformed at startup | Reject rollback | LKG retained |
| Same generation, same decoded semantics | Serve | Unchanged; clear error | No swap/counter increment |
| Same generation, different semantics | Serve initial | Reject fork | LKG retained |
| Higher valid generation, runtime build fails | N/A | Reject | LKG retained |
| Higher valid same-CA generation | Serve initial | Activate once | New work uses new leaf |
| Watcher returns, panics, or is cancelled | N/A | Supervised process error | No silent frozen renewal |

## Status and diagnostics

Watched distributor status extends `transport_security` with an `identity`
object. Control status exposes the equivalent object as
`service_authentication.trust_policy_tls_identity`:

```json
{
  "mode": "watched-bundle",
  "identity_id": "node-a",
  "purpose": "client",
  "bundle_generation": 2,
  "successful_activations": 1,
  "rejected_reloads": 9,
  "last_error_kind": null
}
```

Static paths report `mode: "static-paths"` with no bundle generation. Status
does not expose a path, PEM, CA bytes, private key, subject, serial number, or
raw parser/verifier error. Counters count distinct reported observations rather
than every poll of unchanged invalid bytes.

## Proof plan

The retained v0.30 proof will:

1. issue distributor A/B, publisher A/B, and control A/B leaves under one
   proof-owned CA, plus isolated wrong-CA, wrong-host, wrong-EKU, expired, and
   mismatched fixtures;
2. reject unsafe/malformed/binding and certificate candidates before startup;
3. start the distributor and three controls from generation 1/A bundles;
4. publish policy g1 with publisher A and observe three normal receipts;
5. exercise live malformed, expired, wrong-host, wrong-EKU, wrong-CA,
   permission, symlink, rollback, and same-generation-fork candidates while
   counters advance exactly and LKG traffic continues;
6. hold one A-established server connection across the distributor A→B swap,
   prove it remains usable, and prove a new handshake presents B;
7. rotate the three control client bundles sequentially, proving new fetches
   and receipts use fresh B-capable clients without changing process identity;
8. publish policy g2 with publisher B, observe three verified receipts, and
   prove snapshot/receipt continuity;
9. retain exact PID/parent/start/executable identity, Raft quorum/term safety,
   real CPU JSON, and incremental SSE through `[DONE]` plus EOF; and
10. replay the checker and SVG renderer byte-identically before writing a
    manifest-last evidence bundle.

The proof can establish restart-free same-CA replacement for this exact local
topology. It cannot establish CA migration, revocation, protected key custody,
multi-host behavior, fleet atomicity, or renewal before every possible expiry.

## Alternatives considered

### Re-read certificate files for every connection

This multiplies filesystem races, repeats expensive parsing, and does not solve
client pool reuse. A validated generation produces one immutable runtime
object instead.

### Watch separate certificate and key files

Separate observations can pair a new certificate with an old key or see a
partial operator update. One whole mode-`0600` object makes generation, leaf,
key, CA, and binding one candidate.

### Mutate a `reqwest` client in place

The client does not expose a safe identity mutation contract, and retaining its
pool would make “new operation uses B” false. Building and swapping the entire
client gives a clear snapshot boundary.

### Close all old connections at activation

That would turn a credential handoff into forced traffic interruption and is
not required for ordinary renewal. v0.30 states the established-connection
limit and reserves emergency cancellation for a later milestone.

### Rotate the CA at the same time

CA migration introduces verifier overlap, trust-root ordering, and downgrade
questions separate from leaf lifetime. Pinning one CA isolates the uncertainty
this release is intended to teach.
