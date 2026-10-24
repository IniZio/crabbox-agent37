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
  workRoot: /root/work
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

`workRoot: /root/work` is required: it puts crabbox's work on the VM's billed disk (see Disk). crabbox takes the work root only from this config key; it overwrites any work root an adapter reports in its lease.

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
| `requestTimeoutSeconds` | Seconds one agent37 API request may take. Positive integer. Default 30. |
| `template` | Template to create VMs from: a workspace template name or an `agent37-` system template. Non-empty string. Default `agent37-ubuntu`. Env `CRABBOX_AGENT37_TEMPLATE` overrides it. |
| `diskGb` | Disk size in GB, sent as `resources.disk`. Whole number from 2 up to 12, 20, 40 or 80 for the standard, fast, large or beast class. Default: that maximum. Env `CRABBOX_AGENT37_DISK_GB` overrides it. See Disk below. |
| `sshKey` | Path to the private SSH key. `<sshKey>.pub` must be registered with the workspace. |
| `cache.ttlDays` | Days a cache seed is kept. Whole number. Default 7. |
| `cache.maxSeeds` | Cache seeds kept per owner, newest first. Whole number. Default 10. |

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

## Disk

An agent37 VM has two disks. `/` is an overlay on a fixed 10 GB OS disk. The billed disk, the one `diskGb` sizes, is mounted at `/root`. Without `external.workRoot`, crabbox works under `/work/crabbox` on `/`, and a build fills 10 GB whatever `diskGb` says. With `workRoot: /root/work` the work lands on the billed disk.

Docker keeps its images under its data root, also on `/` by default. On a VM it creates, `acquire` writes `"data-root": "/root/docker"` into `/etc/docker/daemon.json` and keeps any other keys there, so a repo that installs or uses Docker stores images on the billed disk. `agent37-ubuntu` already ships that setting. A VM has no init system: agent37 starts dockerd at boot, and stopping dockerd reboots the VM. So if dockerd already runs on another root, `acquire` stops and starts the VM before it reads the host key. If Docker is still on another root afterwards, or the file cannot be merged, acquire fails and deletes the VM. A VM that already exists is left as it is.

The disk is created at the class maximum unless `diskGb` says otherwise. Disk bills at $0.09 per GB per month, so the largest, 80 GB, costs $7.20 a month.

agent37 can grow a disk later (`POST /v1/instances/{id}/resize`). On a VM every resize is a cold boot that ends every process on the Box, so the adapter never resizes a running VM. The filesystem at `/root` grows into the new space on that boot and its data is kept. A future option, not built: resize a stopped VM before `resume` starts it, since that boot happens anyway.

## Cache volumes

With `external.capabilities.cacheVolumes: true` in trusted user config, crabbox sends `desired.cacheVolumes` on acquire, and a released Box becomes the cache of the next one.

- Each volume's data lives on the billed disk at `/root/.crabbox-cache/<group>/<name>/<path>`. Every declared path is a symlink to it. A VM has no systemd and ignores fstab, and bind mounts are lost on reboot, but symlinks survive a stop and start. agent37's init bind-mounts `/root/docker` onto itself at boot. Through the symlink it binds the store directory onto itself, so Docker's data lands in the store. On a fresh Box, acquire unmounts `/root/docker` and moves the directory into the store, so a running dockerd keeps its files until the reboot that follows.
- `release` of a running Box with cache volumes keeps it as a seed. For each sealed volume that lacks `.crabbox-sealed` in any of its paths, it empties that volume. It removes everything under `/root` except `.crabbox-cache`, `.ssh`, agent37's `.agent37` and the links into the store. A mount point there, such as the `/root/docker` that agent37's init binds when no volume replaced it, is emptied and kept. agent37's stop halts the VM without signalling its processes, and dockerd cannot be stopped first: its exit takes the VM down. So release freezes `/root` with `fsfreeze`, which flushes it and holds later writes. Then it stops the Box, renames it `crabbox-<owner>--seed.<generation>`, and deletes older seeds with the same key. Outside `/root` the disk is kept as it is. If any step fails, the Box is deleted and release still succeeds.
- The seed key is `desired.cacheScope` plus the owner, template, cpu, memory, disk and region; groups are not part of it. A seed carries the whole Box, so a Box of one scope never reuses another scope's seed. `cacheScope` must be 16 lowercase hex. Without it, or with any other value (reported on stderr), acquire never claims a seed, the Box still gets its links and echoes `empty`, and release deletes it.
- `acquire` with cache volumes, a scope and no VM for the lease claims the newest stopped seed of its key and starts it. It clears the seal markers and links any new volume. It echoes `restored` for volumes whose directory the seed held and `empty` for the rest, such as a volume of a group the seed never had. A cold Box echoes `empty`, and a volume the request got wrong echoes `skipped`.
- agent37 has no conditional write. A claim renames the seed and rewrites its metadata to the new lease with `PATCH /v1/instances/{id}`, then reads it back. A lock file in the temp dir serialises claims on one host; without the lock, acquire goes cold. Two hosts can both pass that read, so the started Box settles it: each acquire runs `ln -s <lease id> /run/crabbox-claim`, which only one can create, and `/run` is a tmpfs that a stop clears. The winner writes its lease to the instance again and reads it back. The loser leaves the Box alone and creates a cold one. A seed that fails after this lease won it inside the Box is deleted. One that fails before is renamed back to a seed and stopped; if it will not stop, it is deleted while it is still that seed or still this lease's.
- agent37 merges `PATCH`ed metadata into the stored keys, whatever its docs say. So `crabbox_role` (`seed` or `lease`) is what tells a seed apart. `list`, `resolve`, `release` and name lookups never see a seed. `cleanup` deletes seeds older than `cache.ttlDays`, those beyond the newest `cache.maxSeeds`, and seeds left not stopped for over an hour since they became seeds (`crabbox_seed_since`). It, and a release deleting older seeds, reads each seed again under the claim lock and deletes it only while it still qualifies.
- A stopped seed bills disk only.

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
- `release` deletes the requested instance after checking it matches the lease. A running Box with cache volumes is kept as a seed instead (see Cache volumes). An instance that is already gone counts as released.
- `touch` succeeds and does nothing.
- `cleanup` deletes this owner's instances that `list` cannot report, such as one left by an interrupted acquire, and expired or surplus cache seeds.
- `pause` stops the instance and returns once it reports stopped. On an already stopped instance it succeeds without calling the API.
- `resume` starts the instance (a cold boot), waits for running and for the VM to accept exec and ssh, and returns the lease with a freshly read SSH host key. The key can change when agent37 moves the instance to another host. On an already running instance it does not call start.
- `pause` and `resume` wait at most `readyTimeoutSeconds`. crabbox sends them only when `external.capabilities.pauseResume: true` is set in trusted user config.

The adapter reads the VM's ed25519 SSH host key over the agent37 exec API as root, for `acquire`, `resolve` (on a running lease) and `resume`. It uses the `HostKey` from the image's sshd config `/.agent37/etc/sshd_config` (on agent37 images, `/.agent37/etc/ssh_host_ed25519_key`) and prints its `.pub` if present. Otherwise it derives the public key on the VM, with `ssh-keygen -y` when installed or by reading the public part of the unencrypted OpenSSH private key file. Only the public key leaves the VM. If the sshd config names no usable key, it falls back to `/etc/ssh/ssh_host_ed25519_key`.

`release`, `list` and `cleanup` touch only this owner's instances: the name must start with `crabbox-<owner>--` and the `crabbox_owner` metadata must match. `pause` and `resume` hold to the same rule and refuse an instance whose id differs from the requested cloudId, or that does not match the lease id or name.

## tunnel

`crabbox-agent37 tunnel <id>.agent37.app` is an SSH `ProxyCommand`: it bridges stdin and stdout to the instance's SSH WebSocket (`wss://<id>-22022.agent37.app/`, key in the `X-Agent37-Key` header). It needs no node or `agent37` CLI.

- The key comes from `AGENT37_API_KEY`, or from the 0600 file named by `AGENT37_KEY_FILE`.
- `CRABBOX_AGENT37_TUNNEL_LOG=<file>` also appends every diagnostic line (stall, resume, slow ping, close or loss, exit summary) to that file, each prefixed with a UTC timestamp, the process id and the host. ssh discards a backgrounded `ProxyCommand`'s stderr, so set it to learn why a tunnel dropped. A server close records its code and reason; a network failure records its I/O error kind or "reset or EOF". The file is created 0600 and appended with whole-line writes, so concurrent tunnels can share it. If it cannot be opened the tunnel warns once on stderr and runs on. The log never holds the API key or any header.
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
