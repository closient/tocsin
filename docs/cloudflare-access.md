# Running behind Cloudflare

The usual shape: a Cloudflare Tunnel carries `status.example.com` to tocsin,
which listens only on loopback; everything is public except `/admin/`,
which Cloudflare Access guards and tocsin verifies again.

```text
browser ──> Cloudflare ──(Access, /admin only)──> cloudflared ──> 127.0.0.1:8080 tocsin
CI, cron ─> Cloudflare ─────────────────────────> cloudflared ──> (bearer tokens)
```

| Path | Who | Guarded by |
|------|-----|------------|
| The public pages and feeds, such as `/releases` and `/feeds/incidents.atom` | everyone | nothing |
| `/api/v1/...` reads | everyone; `status:read` tokens see non-public projects | tocsin's tokens |
| `/api/v1/...` writes (incidents, releases, heartbeat pings) | CI, cron jobs, scripts | tocsin's tokens, by scope |
| `/admin/` | the people who run incidents | Cloudflare Access, then tocsin checking the Access token |

API writes stay outside Access: the jobs that make them carry a
`[[token]]` already, and a heartbeat ping from a cron job should not need
an Access service token too.

## 1. Listen on loopback

```toml
[server]
listen = "127.0.0.1:8080"                  # the default
public_url = "https://status.example.com"  # links, emails, and the admin forms' origin
```

With nothing else able to connect, every request has come through the
tunnel. tocsin does not rely on that for `/admin/`, but it keeps the origin
off the internet. In a container, publish the port on loopback only
(`-p 127.0.0.1:8080:8080`), or run `cloudflared` on the same Docker network
and point it at `http://tocsin:8080`.

## 2. The tunnel

Create a tunnel and route the hostname to it:

```sh
cloudflared tunnel create tocsin
cloudflared tunnel route dns tocsin status.example.com
```

```yaml
# /etc/cloudflared/config.yml
tunnel: tocsin
credentials-file: /etc/cloudflared/<tunnel-id>.json
ingress:
  - hostname: status.example.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

A remotely managed tunnel (created in the Zero Trust dashboard or with
Terraform's `cloudflare_zero_trust_tunnel_cloudflared`) works the same:
one public hostname, service `http://127.0.0.1:8080`.

## 3. Access for `/admin`

In Zero Trust, **Access → Applications → Add an application →
Self-hosted**:

- **Domain** `status.example.com`, **path** `admin`. Access protects the
  path and everything below it; the rest of the host stays public.
- **Policy**: allow the people who run incidents, such as an email domain
  or an identity-provider group.
- Copy the **Application Audience (AUD) Tag**.

## 4. Tell tocsin

```toml
[server.auth]
mode = "cloudflare_access"
team_domain = "example"            # example.cloudflareaccess.com
audience = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2"
allowed_domains = ["example.com"]  # optional, on top of the Access policy
```

tocsin verifies the `Cf-Access-Jwt-Assertion` token Access adds to every
request it lets through: its signature against the team's published keys,
its issuer, its audience and its expiry. It never trusts the
`Cf-Access-Authenticated-User-Email` header, so a request that reaches the
origin some other way cannot name a person. tocsin fetches the keys from
`https://example.cloudflareaccess.com/cdn-cgi/access/certs`, so it needs
outbound HTTPS to Cloudflare.

[admin.md](admin.md) covers key refresh, what happens when Cloudflare is
unreachable, and the threat model.

## 5. Check it

```sh
tocsin config check --config /etc/tocsin
curl -sI https://status.example.com/releases | head -1          # 200, no sign-in
curl -sI https://status.example.com/admin/ | grep -i '^location' # a redirect to Access
```

Then sign in through a browser and open `/admin/`. If tocsin refuses a
request Access let through, its log says why: the wrong `audience`, the
wrong `team_domain`, or keys it could not fetch.

## Monitoring through Cloudflare

A check against `https://status.example.com/...` measures Cloudflare and
the tunnel as well as the service. To watch tocsin itself from somewhere
else, point another monitor at the public URL; to watch your services from
tocsin, check them directly rather than through this tunnel.
