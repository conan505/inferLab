# Phase 35: Restart-free same-CA mTLS leaf renewal

## What we are learning

This phase asks one narrow question:

> How can the trust distributor and its clients replace their TLS leaf
> identities without restarting, changing CA trust, or pretending existing
> connections were re-handshaken?

v0.24 established TLS 1.3 mutual authentication for the trust-distribution
channel. v0.29 then established a whole-object, last-known-good handoff pattern
for Ed25519 service signers. v0.30 applies that lifecycle discipline to X.509
leaf identities while preserving the important difference between a server
handshake configuration and a client connection pool.

## Mental model: badges at a guarded archive

Imagine an archive with one guarded entrance and several couriers:

- the archive's wall plaque is the **server leaf**;
- each courier badge is a **client leaf**;
- the badge office seal is the **issuer CA**;
- the guard's trusted seal list is the **verification CA**;
- the sealed badge packet is an **identity bundle**; and
- the packet number is the **bundle generation**.

The archive can hang plaque B for people who arrive next. Someone already
inside saw plaque A when entering; replacing the plaque does not replay their
entrance check. Likewise, a courier already on a trip may finish with badge A.
A new courier trip after the handoff takes badge B and a fresh vehicle rather
than joining an old vehicle pool.

```mermaid
flowchart LR
    BundleA["bundle g1<br/>leaf A + key + issuer CA"] --> RuntimeA["runtime A"]
    Issuer["generation-1 issuer CA pin"] --> Check{"same CA + valid time<br/>EKU + name + key?"}
    BundleB["bundle g2<br/>leaf B + key + same issuer CA"] --> Check
    Check -->|"yes"| RuntimeB["runtime B"]
    Check -->|"no"| RuntimeA
    RuntimeA --> Old["established/in-flight work stays A"]
    RuntimeB --> New["new handshake/client snapshot uses B"]
```

## The problem without renewal

A leaf certificate has a finite validity window. If every process reads its
certificate only at startup, ordinary renewal becomes a process-replacement
event. For InferLab that means replacing the distributor or control for a
channel credential even though its Raft state, caches, queues, and application
keys are still valid.

Blindly rereading PEM files is not enough:

- separate certificate/key files can be observed between writes;
- a valid certificate can be for the wrong hostname or TLS purpose;
- a candidate can silently move to a different CA;
- an old generation can roll a process backward;
- a client can keep using an old pooled TLS connection after its files change;
- and status can claim renewal merely because new bytes appeared on disk.

The design needs a precise transition from one complete usable runtime identity
to another.

## Vocabulary

| Term | Plain-language meaning | What it is not |
|---|---|---|
| Leaf | One end-entity certificate and matching private key | A CA |
| Chain | Leaf followed by any intermediate certificates | An unordered bag |
| Issuer CA | Root set that validates this local leaf | Permission to publish policy |
| Verification CA | Root set used to authenticate the remote peer | Automatically rotated here |
| Purpose | Server-auth or client-auth certificate use | An InferLab endpoint role |
| SAN | DNS name carried by a server certificate | A service credential ID |
| Bundle generation | Ordering for one running identity | Durable global PKI epoch |
| LKG | Last successfully published runtime identity | Last file observed |
| Pool | Reusable outbound connections owned by one HTTP client | Safe to retain across identity change |

## Why one whole bundle

Certificate, private key, issuer CA, identity metadata, and generation describe
one candidate. Watching them separately creates combinations that the operator
never intended. The complete JSON bundle is written beside the live path,
given mode `0600`, and atomically renamed into place.

The loader rejects symlinks and verifies that the file inspected before open is
the same file read. It bounds the entire JSON and each PEM section. This is not
a secrets manager—the private key still resides on local disk and in process
memory—but it gives the local handoff one coherent source.

The issuer CA is included so the initial leaf can be validated before service
and then pinned. A candidate carrying a different CA cannot redefine its own
trust boundary merely by being internally consistent.

## The four validation questions

### 1. Is this the intended object?

Schema, cluster, stable identity ID, purpose, and server name must match the
process configuration exactly. A perfectly valid `client` certificate is not
a valid distributor `server` candidate.

### 2. Is the X.509 identity usable now?

The chain must parse, the private key must match, and the leaf must validate at
the current time under its embedded issuer CA. Server leaves require
server-auth usage and the configured DNS SAN. Client leaves require
client-auth usage.

### 3. Is it an allowed transition?

Generation must be positive. A lower generation is rollback. Equal generation
with equal decoded semantics is unchanged; equal generation with different
semantics is a fork. A higher generation must retain the original issuer-CA
set.

### 4. Can the complete runtime replacement be built?

The distributor builds a full TLS 1.3 mTLS server configuration. A control
builds a full HTTPS client including its identity, server roots, redirect
policy, and a new empty pool. Only a successfully built object can be
published.

## Why server and client handoffs differ

The server selects a configuration when a new TLS handshake begins. Swapping
the configuration pointer does not alter a connection whose handshake already
completed. That old connection can remain safe for ordinary overlap renewal
because A and B are both CA-valid during the handoff.

The control side has an extra trap: its HTTP client owns a connection pool. If
code changed only the certificate source but reused that client, a later fetch
could reuse an A-authenticated connection. The control therefore swaps the
whole client. Each operation clones the current client once; the clone keeps
its old pool alive only for that operation, while operations starting after
publication can see only the new client and pool.

```mermaid
sequenceDiagram
    participant F1 as "fetch already started"
    participant C as "reloadable client slot"
    participant W as "bundle watcher"
    participant F2 as "next receipt/fetch"

    F1->>C: "snapshot client A"
    W->>W: "validate and build client B"
    W->>C: "publish client B"
    F1-->>F1: "finish entirely through client A"
    F2->>C: "snapshot client B"
    F2-->>F2: "new pool; present leaf B"
```

## Invariants

1. The configured verification CAs never change during a v0.30 handoff.
2. Generation 1 pins the local leaf issuer CA for the process lifetime.
3. No candidate is published before binding, key, chain, time, EKU, SAN, CA,
   ordering, and runtime-construction checks pass.
4. A failed observation changes neither the current server config nor the
   current control client.
5. Same-generation formatting changes do not count as activation; changed
   decoded identity at the same generation is a fork.
6. A new control operation snapshots one client and uses it through completion.
7. A control activation creates a new pool; new work cannot fall back to the
   old pool.
8. Existing server connections and in-flight control operations may retain A;
   no status or proof calls that a failure.
9. Watcher counters and last error describe distinct processed observations,
   not poll frequency.
10. Watcher failure is process-supervised rather than silently freezing the
    credential.
11. X.509 channel authentication never replaces root-signed policy or
    service-signed receipt verification.

## State machine in plain language

At startup, only a valid generation-1 bundle opens service. The process records
its decoded issuer CA and runtime identity as LKG.

While serving:

- the same valid object is a no-op;
- unreadable sources are retried;
- stable invalid bytes are reported once;
- rollback and fork candidates are rejected;
- a higher leaf under another CA is rejected;
- a higher valid same-CA leaf builds a complete replacement; and
- publication changes future handshakes or future client snapshots.

Restart creates a new in-memory generation floor and CA pin from the startup
bundle. Durable TLS anti-rollback is not claimed.

## Failure experiment

A useful design must survive more than a happy-path A→B rename. The v0.30
experiment tries to disprove it with:

- malformed JSON and PEM;
- a bundle larger than its bound;
- unsafe permissions and a symlink;
- wrong cluster, identity, purpose, and server name;
- a mismatched private key;
- expired and not-yet-valid certificates;
- wrong server/client EKU;
- wrong server SAN;
- a valid leaf under another CA;
- generation rollback and same-generation fork; and
- a valid higher bundle after all preceding failures.

Every live rejection must leave A or B usable as appropriate, advance only a
bounded diagnostic counter, and preserve process identity. Recovery must clear
the last error without resetting the historical rejection count.

The handoff experiment also holds one server connection across rotation. If
that connection suddenly claims B, the proof model is wrong; TLS did not
re-handshake. A separately opened connection must see B. Controls then rotate
one at a time and must continue fetching policy and posting receipts with new
client pools.

## What the result can teach us

If the experiment passes, it demonstrates three reusable lessons:

- credential rotation is a state-machine problem, not a file-copy problem;
- “new work” needs an explicit snapshot boundary, especially when pools cache
  authenticated connections; and
- truthful overlap semantics are safer than claiming instantaneous global
  replacement.

It does not prove automated issuance, emergency revocation, or CA migration.
Those require distinct authority and failure models and should remain separate
milestones.

## Alternatives and why they lose

| Alternative | Why it loses for this phase |
|---|---|
| Restart each process | Couples leaf lifetime to unrelated runtime availability/state |
| Watch cert and key separately | Allows mixed observations and partial updates |
| Trust the CA named by every candidate | Lets a candidate redefine the trust boundary |
| Keep the old client pool | Makes “new operation uses B” unverifiable |
| Kill all A connections | Adds emergency-cancellation behavior to ordinary renewal |
| Rotate CA and leaf together | Combines two major uncertainties and obscures failures |

## Boundaries to remember

- Private keys remain local files and process memory.
- Old configurations and client clones may retain old key material until their
  references drop; immediate erasure is not claimed.
- Existing TLS connections remain authenticated as they were at handshake.
- The generation/CA floor is process-local and resets on restart.
- Renewal is sequential, not fleet-atomic.
- Certificate expiry can still cause failure if operators do not activate a
  successor in time.
- CRL/OCSP, ACME, HSM/KMS, CA migration, global mTLS, HA, and emergency
  cancellation remain future work.
