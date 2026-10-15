# crabbox-agent37

An agent37 VM provider for crabbox. It uses the crabbox external provider protocol v1.

Each call reads one JSON request on stdin and writes one JSON response on stdout. Diagnostics go to stderr. On failure it exits 1 and prints `{"error": ...}`.

## Scope

- It manages agent37 `type: vm` instances only. It never uses the default gVisor type.
- It never sets instance environment variables.
- It never grants managed-LLM budget and never calls managed-services or budget endpoints.

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

## Operations

- `doctor` checks that the workspace is reachable, that VMs are available, and that the SSH key is registered. It is read-only. It only lists instances and SSH keys. It never creates, changes or deletes anything.
  - If the workspace already has a `type: vm` instance, doctor reports "VM instances available".
  - Otherwise doctor reports that VM availability is unknown until the first acquire. This is a note, not a failure.
  - agent37 answers a VM create on an unfunded workspace with 403 `tier_limit`. The adapter reports this as "workspace not funded for VM instances".

- `acquire` finds or creates the VM for the lease, waits until it runs and accepts exec, and returns the lease with the VM's SSH host key. It never starts a stopped VM. On a stopped instance it fails with "instance <id> is stopped; run crabbox resume".
- `resolve` looks up the existing VM for a lease and returns it. It never creates or deletes. It reports a stopped lease without reading its host key, which is what crabbox needs before `pause` and `resume`.
- `list` returns this owner's VM leases, sorted by name, then cloud id.
- `release` deletes the requested instance after checking it matches the lease. An instance that is already gone counts as released.
- `touch` succeeds and does nothing.
- `cleanup` deletes this owner's instances that `list` cannot report, such as one left by an interrupted acquire.
- `pause` stops the instance and returns once it reports stopped. On an already stopped instance it succeeds without calling the API.
- `resume` starts the instance (a cold boot), waits for running and for the VM to accept exec and ssh, and returns the lease with a freshly read SSH host key. The key can change when agent37 moves the instance to another host. On an already running instance it does not call start.
- `pause` and `resume` wait at most `readyTimeoutSeconds`. crabbox sends them only when `external.capabilities.pauseResume: true` is set in trusted user config.

`release`, `list` and `cleanup` touch only this owner's instances: the name must start with `crabbox-<owner>--` and the `crabbox_owner` metadata must match. `pause` and `resume` hold to the same rule and refuse an instance whose id differs from the requested cloudId, or that does not match the lease id or name.

## tunnel

`crabbox-agent37 tunnel <id>.agent37.app` is an SSH `ProxyCommand`: it bridges stdin and stdout to the instance's SSH WebSocket (`wss://<id>-22022.agent37.app/`, key in the `X-Agent37-Key` header). It needs no node or `agent37` CLI.

- The key comes from `AGENT37_API_KEY`, or from the 0600 file named by `AGENT37_KEY_FILE`.
- Any other host exits 1. A server close or network loss also exits nonzero; ssh exiting (stdin EOF) exits 0.
- `ssh -o ProxyCommand="crabbox-agent37 tunnel %h" root@<id>.agent37.app` is the manual check against a real instance.

## Development

```
make setup
make ci
```

## License

MIT
