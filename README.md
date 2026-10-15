# crabbox-agent37

An agent37 VM provider for crabbox. It uses the crabbox external provider protocol v1.

Each call reads one JSON request on stdin and writes one JSON response on stdout. Diagnostics go to stderr. On failure it exits 1 and prints `{"error": ...}`.

## Install

Download the archive for your platform from the [GitHub releases](https://github.com/IniZio/crabbox-agent37/releases) of IniZio/crabbox-agent37. Archives are named `crabbox-agent37-<tag>-<target>.tar.gz`. Targets are `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`.

Verify the download with the `SHA256SUMS` file from the same release, then unpack it and put the binary on your `PATH`:

```
shasum -a 256 -c SHA256SUMS --ignore-missing   # or: sha256sum -c SHA256SUMS --ignore-missing
tar -xzf crabbox-agent37-<tag>-<target>.tar.gz
install -m 0755 crabbox-agent37-<tag>-<target>/crabbox-agent37 ~/.local/bin/
```

To build from source instead:

```
cargo install --git https://github.com/IniZio/crabbox-agent37 --locked
```

You need the IniZio/crabbox fork release with external `pauseResume` and host-key support. Upstream crabbox does not have these.

## Configure crabbox

Put this in your crabbox user config (the file `crabbox config path` prints):

```yaml
provider: external
target: linux
external:
  command: /home/you/.local/bin/crabbox-agent37
  capabilities:
    idempotentLeaseId: true
    pauseResume: true
  connection:
    ssh:
      trustProviderOutput: true
  config:
    owner: myteam
    keyFile: /home/you/.config/agent37/api-key
    region: us
    sshKey: /home/you/.ssh/agent37_team
```

`capabilities.pauseResume` and `connection.ssh.trustProviderOutput` are honoured only in trusted user config. Repository config, environment variables and flags cannot enable them. Use an absolute path for `command`.

The keys under `config` are described in the Config table below.

## SSH key for a shared workspace

agent37 SSH keys belong to the whole workspace, at most 20 per workspace. A registered key gives root on every instance in the workspace. Use one team key pair for the workspace.

1. Create the pair once: `ssh-keygen -t ed25519 -f ~/.ssh/agent37_team`.
2. Register `agent37_team.pub` once, in the agent37 dashboard or with `agent37 ssh setup`.
3. Set `config.sshKey` to the private key path.
4. Run `crabbox doctor`. The adapter's `doctor` reports whether the key is registered.

## API key

The key comes from `AGENT37_API_KEY` or from a file named by `config.keyFile`. When `keyFile` is set, the file wins. The file must have mode 0600. A file with a wider mode is refused.

Do not put the key in crabbox config. One team key is held on the controller host.

## Config

| Key | Meaning |
| --- | --- |
| `apiUrl` | agent37 API URL. Default `https://api.agent37.com`. |
| `keyFile` | Path to a 0600 file that holds the API key. |
| `owner` | Required. Label for your instances: lowercase letters, digits and single dashes, at most 32 characters. Instances are named `crabbox-<owner>--<name>`. |
| `region` | `us` (default) or `eu`. |
| `readyTimeoutSeconds` | Seconds to wait for a running VM, and for pause and resume. Positive integer. Default 300. |
| `sshKey` | Path to the private SSH key. `<sshKey>.pub` must be registered with the workspace. |

## Rules

- It manages agent37 `type: vm` instances only. It never uses the default gVisor type. Default instances cannot run nono (Landlock) or Docker.
- It never sets instance environment variables. Any process inside an instance can read its env vars.
- Each instance gets an `AGENT37_MANAGED_TOKEN`. Its managed-LLM budget defaults to $0. The adapter never raises it and never calls managed-services or budget endpoints. Do not grant budget on the workspace.
- One team API key is held on the controller host only.

## Limits and regions

Instance caps depend on the workspace funding tier:

| Tier | Instances |
| --- | --- |
| Before the first top-up | 1 |
| After the first top-up | 10 |
| $100 total top-ups | 50 |
| $250 total top-ups | 200 |

Set moor's per-provider cap to match your tier. Regions are `us` (default) and `eu`.

VM instances cost $11.36 to $90.16 per month always-on, from 2 vCPU/4 GB to 16 vCPU/32 GB. Billing is per minute. A stopped instance bills disk only.

## Sleep

agent37 auto-sleep stays off: the adapter creates instances with auto-sleep disabled. moor sleeps the Box with `crabbox pause` and wakes it with `crabbox resume`. Resume is a cold boot. The host key may change, and crabbox pins the new one.

## Operations

- `doctor` checks that the workspace is reachable, that VMs are available, and that the SSH key is registered. It is read-only. It only lists instances and SSH keys. It never creates, changes or deletes anything.
  - The SSH key check compares the SHA256 fingerprint of the local `<sshKey>.pub` (as `ssh-keygen -lf` prints it) with the fingerprints from `GET /v1/ssh-keys`.
  - If the workspace already has a `type: vm` instance, doctor reports "VM instances available".
  - Otherwise doctor reports that VM availability is unknown until the first acquire. This is a note, not a failure.
  - agent37 answers a VM create on an unfunded workspace with 403 `tier_limit`. The adapter reports this as "workspace not funded for VM instances".

- `acquire` finds or creates the VM for the lease, waits until it runs and accepts exec, and returns the lease with the VM's SSH host key. If acquire fails after it created a VM, it deletes that VM, even when crabbox asked to keep it. Keep applies only to a lease that acquire returned. It never deletes a VM it did not create in that call. It never starts a stopped VM. On a stopped instance it fails with "instance <id> is stopped; run crabbox resume".
- `resolve` looks up the existing VM for a lease and returns it. It never creates or deletes. It reports a stopped lease without reading its host key, which is what crabbox needs before `pause` and `resume`.
- `list` returns this owner's VM leases, sorted by name, then cloud id.
- `release` deletes the requested instance after checking it matches the lease. An instance that is already gone counts as released.
- `touch` succeeds and does nothing.
- `cleanup` deletes this owner's instances that `list` cannot report, such as one left by an interrupted acquire.
- `pause` stops the instance and returns once it reports stopped. On an already stopped instance it succeeds without calling the API.
- `resume` starts the instance (a cold boot), waits for running and for the VM to accept exec and ssh, and returns the lease with a freshly read SSH host key. The key can change when agent37 moves the instance to another host. On an already running instance it does not call start.
- `pause` and `resume` wait at most `readyTimeoutSeconds`. crabbox sends them only when `external.capabilities.pauseResume: true` is set in trusted user config.

The adapter reads the VM's ed25519 SSH host key over the agent37 exec API as root, for `acquire`, `resolve` (on a running lease) and `resume`. It uses the `HostKey` from the image's sshd config `/.agent37/etc/sshd_config` (on agent37 images, `/.agent37/etc/ssh_host_ed25519_key`) and prints its `.pub` if present. Otherwise it derives the public key on the VM, with `ssh-keygen -y` when installed or by reading the public part of the unencrypted OpenSSH private key file. Only the public key leaves the VM. If the sshd config names no usable key, it falls back to `/etc/ssh/ssh_host_ed25519_key`.

`release`, `list` and `cleanup` touch only this owner's instances: the name must start with `crabbox-<owner>--` and the `crabbox_owner` metadata must match. `pause` and `resume` hold to the same rule and refuse an instance whose id differs from the requested cloudId, or that does not match the lease id or name.

## tunnel

`crabbox-agent37 tunnel <id>.agent37.app` is an SSH `ProxyCommand`: it bridges stdin and stdout to the instance's SSH WebSocket (`wss://<id>-22022.agent37.app/`, key in the `X-Agent37-Key` header). It needs no node or `agent37` CLI.

- The key comes from `AGENT37_API_KEY`, or from the 0600 file named by `AGENT37_KEY_FILE`.
- Any other host exits 1. A server close or network loss also exits nonzero; ssh exiting (stdin EOF) exits 0.
- `ssh -o ProxyCommand="crabbox-agent37 tunnel %h" root@<id>.agent37.app` is the manual check against a real instance.

## Live smoke

From a checkout of the crabbox fork, with the user config above in place:

```
CRABBOX_LIVE=1 CRABBOX_LIVE_PROVIDERS=external scripts/live-smoke.sh
```

This creates and deletes a real billed VM. The script reads `external.command` from your user config. Optional overrides:

- `CRABBOX_BIN`: crabbox binary to test. Default `bin/crabbox` in the checkout.
- `CRABBOX_CONFIG`: config file to use instead of the user config.
- `CRABBOX_LIVE_EXTERNAL_COMMAND`: adapter command, instead of the config value.
- `CRABBOX_LIVE_EXTERNAL_ARG`, `CRABBOX_LIVE_EXTERNAL_WORK_ROOT`, `CRABBOX_LIVE_EXTERNAL_SLUG`, `CRABBOX_LIVE_EXTERNAL_WAIT_TIMEOUT`: adapter argument, work root, slug and wait time (default 5m).
- `CRABBOX_LIVE_COMMAND`: command to run on the VM.

The API key must be available to the adapter (`keyFile` or `AGENT37_API_KEY`).

## Development

```
make setup
make ci
```

## License

MIT
