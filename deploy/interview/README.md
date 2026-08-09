# Interview topology modes

The default flow remains local and zero-cost:

```sh
./deploy/interview/start.sh
```

It preserves the historical single loopback gateway listener and uses visibly
public local fixture keys. Authenticated diagnostics remain available through
that local-only listener. Those fixture values are for rehearsal only.

To rehearse the stricter hosted-edge contract, install the template with mode
`0600` at a path outside the repository, replace every placeholder, load it
into the current shell, and start the strict mode:

```sh
install -m 600 deploy/interview/hosted-edge.env.example /absolute/private/path/inferlab-hosted-edge.env
set -a
. /absolute/private/path/inferlab-hosted-edge.env
set +a
./deploy/interview/start.sh --hosted-edge
```

Treat that loaded shell environment and access to Docker inspection as trusted
surfaces: either can reveal the injected values. This rehearsal does not
replace a deployment secret manager.

Hosted-edge mode requires explicit, distinct public/operator credentials and a
matching route-signing private/public key pair. The public listener remains on
the configured host loopback port in this local rehearsal. The operator
listener is never host-published. Public `/internal/*` requests return 404;
the private listener exposes only `/internal/workers` behind the operator key.
The `${VAR:?}` checks in `compose.hosted-edge.yaml` reject only missing or empty
values. Before Compose runs, the start script separately parses credential
lists and rejects template placeholders, checked-in API values in either role,
and checked-in route-signing key material. Running the base Compose file alone
always selects `local` mode.

This mode rehearses the application boundary needed for deployment; it does
**not** provide or claim public DNS, internet hosting, TLS termination,
DDoS/WAF protection, distributed rate limiting, a secret manager, or a
hosting-provider free tier.
Place the public listener behind provider-managed HTTPS and network controls
before exposing it outside a trusted machine.

Stop local or hosted rehearsal mode while retaining state with the matching
command:

```sh
./deploy/interview/stop.sh                # local rehearsal
./deploy/interview/stop.sh --hosted-edge  # hosted-edge rehearsal
```

Both commands target the same Compose project, so teardown does not require the
hosted secret environment to remain loaded. Add `--purge-data` only when
intentionally deleting the dedicated interview volumes.
