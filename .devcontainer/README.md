# Development container

The image-based configuration is designed for DevPod with local rootless Podman.
It keeps Cargo, rustup, mise, and editor caches in named volumes and forwards the
1Password SSH agent when the host socket exists. No Compose stack is needed
because immortal has no development-time backing services.
Mise also installs the pinned yamllint release used by local and hosted CI.

```sh
scripts/dev-up
scripts/dev-ssh
just ci
```
