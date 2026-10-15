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
| `sshKey` | Path to the private SSH key. `<sshKey>.pub` must be registered with the workspace. |

## Operations

- `doctor` checks that the workspace is reachable, that VMs are available, and that the SSH key is registered.
  - agent37 answers 403 `tier_limit` when the workspace cannot run VMs. Doctor reports this as "workspace not funded for VM instances".
  - agent37 has no read-only way to check VM availability. When the workspace has no VM yet, doctor creates a small probe VM named `crabbox-agent37-doctor-probe` and deletes it at once. This costs a few seconds of VM time.

All other operations return an error for now.

## Development

```
make setup
make ci
```

## License

MIT
