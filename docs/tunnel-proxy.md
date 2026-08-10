# Serving tunnels: what the proxy in front has to do

`nook tunnel 3000` publishes a port on a machine in the fleet at
`<label>.<TUNNEL_DOMAIN>`. Whether that URL resolves is not up to NookOS — it is
up to whatever terminates TLS in front of the control plane, and on the routing
rule you give it for the tunnel zone.

**Nothing in this repository writes that rule for you yet, on any deployment.**
Two of the shipped modes have a proxy the contract *can* be satisfied on —
Compose behind Traefik, and the Helm chart's ingress — but neither emits a
wildcard router or a wildcard ingress rule, and neither sets `TUNNEL_DOMAIN`;
automating each is its own card. So this page is for every deployment, not for
"everyone else": on those two you are adding a rule to a proxy that already
exists, and on the rest you are standing one up first.

The failure when it is missing is quiet: `nook tunnel` hands back a URL and the
URL does not resolve — or resolves to NookOS's own app shell instead of the app
on the port. Nothing logs an error, because from the control plane's point of
view no request ever arrived.

## The contract

**A request whose `Host` is `<label>.<TUNNEL_DOMAIN>` must reach the control
plane, on every path, with the original `Host` intact, and with `Upgrade` and
`Connection` forwarded.**

Four parts, each of which is a separate way to get this wrong.

**The control plane, not the web service.** A NookOS deployment runs two HTTP
services: the control plane (`nook-control`) and the SPA host (`nook-web`). The
*apex* is split between them by path: the shipped Traefik router sends `/api`,
`/mcp`, `/healthz`, `/.well-known` and `/install.sh` to the control plane and
everything else to the SPA (`crates/nook-node/src/wizard/generate.rs`). Copying
that split onto the wildcard is the natural mistake, and it breaks every tunnel,
because a tunnel serves the app's own root: `/` on a tunnel host must reach the
control plane, and under the apex's rules `/` is the SPA's.

Sending the wildcard to `nook-web` instead does not fail loudly either, which is
why this is worth stating twice. That image makes the same split internally —
`/api`, `/mcp`, `/healthz` and `/.well-known` to the control plane — and its
catch-all is an SPA fallback, `try_files $uri $uri/ /index.html`
(`deploy/docker/nginx.conf.template`). So **every** path on a tunnel host comes
back `200` with the NookOS app shell in it. The browser shows NookOS where the
app on the port should be, and nothing anywhere reports an error.

**Every path, with no exceptions carved out.** On the control-plane side the
decision is made by `host_dispatch`, a layer that runs *before* routing
(`crates/nook-control/src/routes/tunnels.rs`): what makes a request a tunnel's
is its host, and the path is not consulted at all. So a proxy that forwards only
`/api` forwards the one prefix a tunnel never uses. The sign-in bounce lands on
`/__nook/tunnel/grant` on the tunnel host, which is not under `/api` either.

**The `Host` header unmodified.** The label in the host is the *only* thing that
says which tunnel a request is for; there is no path prefix, no header and no
cookie carrying it. A proxy that rewrites `Host` to the upstream's address — the
nginx default — turns every tunnel request into an ordinary API request for
`127.0.0.1:8080`, which is answered by the API rather than by the tunnel.

**`Upgrade` and `Connection` forwarded.** The same control plane serves the
apex's WebSockets — the UI event stream, the node listener and terminal attach
(`/api/v1/ws/ui`, `/api/v1/ws/node`, `/api/v1/ws/sessions/{id}/attach`) — so a
proxy that drops upgrade headers breaks the product whether or not it breaks
tunnels. See [what a tunnel actually carries](#what-a-tunnel-actually-carries)
for the narrower question of upgrades *through* a tunnel.

### Two things that are not requirements

**No session affinity.** Tunnel routes are broadcast between control-plane
replicas, and a replica that does not hold the node's socket forwards the
exchange to the one that does (`crates/nook-control/src/ws/registry.rs`). A load
balancer may send any request to any replica.

**No rule for `/__nook/`.** Everything under that prefix on a tunnel host
belongs to NookOS and is never forwarded to the application behind the port.
It needs no configuration; it just must not be excluded.

## What the proxy config cannot supply

Three prerequisites live outside the proxy. All three have to be true before any
`server` block below does anything useful.

**1. `TUNNEL_DOMAIN` on the control plane.** The whole surface is off unless it
is set: with no value, no host is treated as a tunnel and `nook tunnel` refuses
by name rather than handing back a URL. It is deliberately *not* inferred from
`PUBLIC_BASE_URL` — inferring it would produce a name that resolves nowhere,
which is precisely the failure this page exists to prevent
(`crates/nook-infra/src/config.rs`).

**2. A wildcard DNS record**, `*.<TUNNEL_DOMAIN>`, pointing at the proxy. Tunnel
labels are minted at runtime from the workspace and node names, so there is no
list of hosts to create records for.

**3. A certificate covering `*.<TUNNEL_DOMAIN>`.** Per-host certificates cannot
work here for the same reason: the hosts are invented when somebody runs `nook
tunnel`, and issuance takes minutes.

Two details about that certificate are worth stating outright, because both are
commonly assumed the other way:

- **A wildcard is one label deep.** `*.example.com` matches
  `tunnels.example.com` and does **not** match `api-azul.tunnels.example.com`.
  If `TUNNEL_DOMAIN=tunnels.example.com`, the certificate you need is
  `*.tunnels.example.com`. Tunnel labels are themselves single DNS labels — the
  slug is built with no dots in it (`crates/nook-proto/src/tunnel.rs`) — so one
  wildcard at that depth covers every tunnel, and nothing needs a deeper one.
- **DNS-01 does not require the wildcard record to exist yet.** Let's Encrypt
  issues wildcards only over the DNS-01 challenge, and that challenge proves
  control of the *zone* by publishing a `_acme-challenge` TXT record. Nothing
  resolves `*.<TUNNEL_DOMAIN>` during issuance, so you can hold the certificate
  before you point the wildcard anywhere — which is the useful order, since the
  proxy needs the certificate to start.

## Deployment modes and what each is missing

A tunnel is a hostname something has to route. **No mode serves tunnels as
shipped**, but they fail at different points: four have no proxy in front of the
control plane at all, and two have one whose configuration stops at the apex.

| Mode | Tunnels | Why |
| --- | --- | --- |
| Compose, ports published directly | **No proxy** | The control plane publishes `8080` on the host itself. Nothing terminates TLS and nothing routes by host. |
| Compose behind Traefik | Proxy yes, **rule no** | Traefik is the proxy, and the generated labels define only the two apex routers. Add a wildcard router yourself. |
| `docker run` | **No proxy** | Same as direct Compose — `-p 8080:8080`, and whatever orchestrates it is expected to bring its own front end. |
| systemd + native binary | **No proxy** | The binary binds its port on the host directly. |
| Kubernetes (Helm chart) | Proxy yes, **rule no** | The ingress is the proxy, and its only rule is the apex. Add a wildcard rule yourself, pointed at the **control-plane** Service. |
| Desktop app (Tauri, SQLite) | **Never** | The bundled control plane is a sidecar on `127.0.0.1` at an OS-assigned port, backed by a SQLite file under the app-data directory. It is not reachable from another machine at all, let alone by hostname. |

The first four are the `Deployment` variants the installer offers
(`crates/nook-node/src/wizard/generate.rs`); the desktop build is separate,
spawning its own `nook-control` bound to loopback
(`frontend/apps/desktop/src-tauri/src/lib.rs`).

On Kubernetes the chart's own ingress rule sends the apex host entirely to the
`web` Service, which proxies `/api` onward itself
(`charts/nook-control/templates/ingress.yaml`). A wildcard rule copied from it
inherits that backend and hits the SPA-fallback trap above, so point the tunnel
host's rule at the control-plane Service directly.

**Only the desktop build is a permanent no.** Its control plane is deliberately
local-only, and there is no hostname to route to a loopback sidecar on another
person's laptop. Every other row becomes tunnel-capable once a proxy satisfying
the contract above is in front of it and `TUNNEL_DOMAIN` is set — for the four
no-proxy modes that means standing one up, which is what the nginx and Caddy
examples below are for; for Traefik and the chart it means one more router or
rule on the proxy you already run.

Today `nook tunnel` returns a URL on all of these regardless. It does not detect
that a deployment has no proxy or no wildcard rule, so the first sign of any of
this is the URL failing to resolve.

## A worked example: nginx

Two blocks. The `map` goes at `http` level, once for the whole config; it is
what makes `Connection: upgrade` conditional, since sending it on every request
breaks keep-alive.

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

server {
    listen 443 ssl;
    http2 on;                       # nginx 1.25+; older: `listen 443 ssl http2;`
    server_name *.tunnels.example.com;

    ssl_certificate     /etc/letsencrypt/live/tunnels.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tunnels.example.com/privkey.pem;

    # The whole path space, to the control plane. Not /api, and not the SPA.
    location / {
        proxy_pass http://127.0.0.1:8080;

        # THE line this all turns on. nginx's default is $proxy_host, which
        # would send `127.0.0.1:8080` and lose the label that names the tunnel.
        proxy_set_header Host $host;

        proxy_http_version 1.1;
        proxy_set_header Upgrade    $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Responses stream chunk by chunk; buffering them here would hold a log
        # tail or an SSE endpoint until it ended.
        proxy_buffering off;

        # The control plane refuses a request body over 32 MiB with a stated
        # limit. Matching it means the refusal comes from NookOS, in NookOS's
        # words, rather than as a bare 413 from nginx.
        client_max_body_size 32m;

        proxy_read_timeout 300s;
    }
}
```

`127.0.0.1:8080` is the control plane as a direct-Compose or systemd deployment
publishes it. On Compose without published ports, use the service name from the
proxy's own network instead (`http://control-plane:8080`).

Your **apex** (`PUBLIC_BASE_URL`) needs its own `server` block, and it is not
optional for tunnels: a browser arriving at a tunnel with no credential is
redirected to `/api/v1/tunnels/authorize` on the apex and sent back to the
tunnel host with a one-minute grant. If the apex is not reachable in that same
browser, the tunnel is not either.

## A worked example: Caddy

Caddy preserves the inbound `Host` on `reverse_proxy` by default and handles
upgrades without configuration, so the contract above needs nothing spelled out.
The one thing it does need is a DNS provider plugin, because a wildcard
certificate means DNS-01 and stock Caddy has no way to write a TXT record:

```caddyfile
*.tunnels.example.com {
	tls {
		dns cloudflare {env.CF_API_TOKEN}
	}
	reverse_proxy 127.0.0.1:8080
}
```

Build Caddy with the provider you use:

```
xcaddy build --with github.com/caddy-dns/cloudflare
```

`request_body { max_size 32MB }` inside the site block is worth adding for the
same reason as nginx's `client_max_body_size`.

## Compose behind Traefik: the router that is missing

`nook server init` generates two routers on the control-plane container —
`nook-api` for the apex's API prefixes and `nook-web` for everything else on the
apex — and no wildcard router. These labels add one, on the same container,
reusing the `nook-api` service the generated file already defines:

```yaml
      - "traefik.http.routers.nook-tunnels.rule=HostRegexp(`^[a-z0-9-]+\\.tunnels\\.example\\.com$`)"
      - "traefik.http.routers.nook-tunnels.entrypoints=websecure"
      - "traefik.http.routers.nook-tunnels.service=nook-api"
      - "traefik.http.routers.nook-tunnels.tls=true"
      # A wildcard means DNS-01, so this resolver must be a DNS-01 one.
      - "traefik.http.routers.nook-tunnels.tls.certresolver=letsencrypt-dns"
      - "traefik.http.routers.nook-tunnels.tls.domains[0].main=tunnels.example.com"
      - "traefik.http.routers.nook-tunnels.tls.domains[0].sans=*.tunnels.example.com"
```

`HostRegexp` takes a Go regular expression in Traefik v3; on v2 the named-group
form ``HostRegexp(`{sub:[a-z0-9-]+}.tunnels.example.com`)`` is the equivalent.
Either way it routes to the control plane, which is what makes this different
from the two generated routers — same `service=nook-api`, without the apex's
path split.

**No `priority` is needed, and that is worth knowing rather than guessing.**
Traefik only weighs priorities between routers whose rules both match a request,
and the generated pair match `Host(<your apex>)` exactly, which no tunnel host
satisfies.

The exception is a `TUNNEL_DOMAIN` that is a *parent* of your apex — apex
`nook.example.com` with `TUNNEL_DOMAIN=example.com`. Do not do that, and not
only because of routing: `host_dispatch` would strip the zone off your own apex,
read `nook` as a tunnel label, and answer the whole application with the "No such
tunnel" page. Give tunnels a zone of their own, beside the apex rather than above
it.

## What a tunnel actually carries

Useful to know before you debug your proxy for something the proxy is not doing.

A tunnel forwards **HTTP request/response**. The request is buffered whole and
sent to the node as a single frame — method, path with query, headers, body —
and the response head comes back followed by streamed body chunks. Requests over
32 MiB are refused with a stated limit rather than being allowed to decide how
much memory a replica spends.

**A WebSocket upgrade does not survive a tunnel today.** Hop-by-hop headers,
including `Upgrade` and `Connection`, are stripped before the node dials the
port, and the wire protocol has no bidirectional stream after the response head
(`crates/nook-control/src/tunnels.rs`, `crates/nook-proto/src/tunnel.rs`). A dev
server behind a tunnel serves its pages; its hot-reload socket will not connect.
This is a property of the tunnel, not of your proxy — forward the upgrade
headers anyway, both because the apex needs them and so that nothing has to
change here when tunnels gain them.

## Checking it

Point the wildcard at the proxy, set `TUNNEL_DOMAIN`, restart the control plane,
then ask for a host under the zone that is *not* a live tunnel:

```
curl -i https://nothing-here.tunnels.example.com/
```

NookOS answers that with its own 404 page — an amber-on-black HTML page headed
"No such tunnel". Getting it means the `Host` reached the control plane with its
label intact and the contract is satisfied. Each other answer names a different
requirement:

| Answer | What is wrong |
| --- | --- |
| `404` with the "No such tunnel" HTML page | Nothing — this is the pass. |
| Connection refused, or NXDOMAIN | The wildcard DNS record. |
| A certificate warning or handshake failure | The wildcard certificate, or a name one label deeper than it covers. |
| `200` carrying the NookOS app shell | The wildcard reaches `nook-web`, not the control plane. |
| `404` with an empty body | The control plane, but with `Host` rewritten — so it was routed as an ordinary API request, and the API has no `/`. This is what the `proxy_set_header Host $host` line prevents. |

Then run `nook tunnel 3000` on a node and open the URL it prints.
