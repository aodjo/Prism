# Osaka deployment

The `CI` workflow publishes development client bundles after all native build jobs, lint,
and the rendezvous image test pass. A `develop` push that changes server sources also builds
the rendezvous image on an ARM64 runner and deploys it to the remaining Japan (Osaka) region.
The former standalone `Deploy` workflow has been removed.

## Connection

`DEPLOY_HOSTS` is a repository variable containing the private management address separately
from the public UDP probe address:

```json
[{"region":"Japan (Osaka)","host":"100.123.15.26","public_host":"161.33.15.29","port":2222,"user":"ubuntu"}]
```

`TAILSCALE_CLIENT_ID` and `TAILSCALE_AUDIENCE` identify an OIDC trust credential. It accepts
GitHub's issuer, subject `repo:aodjo/Prism:ref:refs/heads/develop`, and the `workflow_ref` claim
`aodjo/Prism/.github/workflows/ci.yml@refs/heads/develop`. Its only write scope is `auth_keys`,
restricted to `tag:prism-deploy`. This tag can reach only Osaka's TCP 2222. Ordinary user-owned
devices retain their existing member access. The workflow's Tailscale node is ephemeral.

`DEPLOY_SSH_KEY` is the dedicated deployment key, and `DEPLOY_KNOWN_HOSTS` pins the existing
management SSH host key. On Osaka, the key's `authorized_keys` entry uses:

```text
restrict,command="sudo /usr/local/sbin/prism-deploy"
```

Install `deploy/rendezvous/deploy.py` at `/usr/local/sbin/prism-deploy`, owned by root and
mode 0755. The key invokes this helper regardless of the SSH command requested. It does not
provide an interactive shell, forwarding, or arbitrary remote commands.

## Deployment and recovery

The helper reads a bounded JSON request on stdin. It accepts only this repository's image,
pinned by both commit and digest, and checks that the resolved Compose configuration affects
only the existing Osaka rendezvous service. Host mounts and extra container privileges are
rejected. The pulled image must report architecture `arm64` before the running stack changes.

Registry credentials are supplied from the job's short-lived `GITHUB_TOKEN`, used through a
temporary Docker configuration, and removed when the helper exits. `/opt/prism/.env` remains
on the server. Deployment does not prune images or change unrelated containers.

The new container must run, bind UDP 47300 and 47301, and answer a local rendezvous probe.
Otherwise the helper restores the previous image and Compose configuration. CI then checks
the region's public UDP response independently. `/opt/prism/.image` records the last successful
image; `.image.previous` and `compose.yaml.previous` retain the previous deployment.

Set repository variable `RENDEZVOUS_DEPLOY_ENABLED` to `false` to pause image publication and
server deployment during maintenance. Client releases continue. Remove the override to resume.

Validate deployment isolation and rollback locally with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s deploy/rendezvous -p 'test_*.py'
actionlint .github/workflows/ci.yml
```
