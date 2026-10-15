# Docker Image Notes

This document records the constraints for Docker runtime profiles launched by `cutex`.

## What cutex does

When a profile uses Docker runtime, `cutex` starts the selected CLI with a command equivalent to:

```sh
docker run --rm -it \
  --user <uid:gid> \
  -e HOME=/home/<user_name> \
  -e USER=<user_name> \
  -e LOGNAME=<user_name> \
  -e CODEX_HOME=/home/<user_name>/.codex \
  -e CODEX_AUTH_FILE=/home/<user_name>/.cutex-profiles/<account-id>/auth.json \
  -e CODEX_CONFIG_FILE=/home/<user_name>/.cutex-profiles/<account-id>/config.toml \
  -e CODEX_CUSTOM_STATUS_ITEMS_FILE=/home/<user_name>/.cutex-profiles/<account-id>/custom-status-items.json \
  -e ALL_PROXY=<effective proxy, when configured> \
  -e NO_PROXY=<effective no_proxy list, when configured> \
  -e CUTE_CODEX_FORCE_HTTP_TRANSPORT=1 \
  -v <host-cwd>:<host-cwd> \
  -w <host-cwd> \
  -v ~/.cutex/runtime/docker-home:/home/<user_name> \
  -v ~/.cutex/profiles:/home/<user_name>/.cutex-profiles \
  <image> \
  <selected-cli>
```

Current behavior:

- The selected CLI name follows the same host-side resolution order as normal launches: `CUTEX_CODEX_BIN`, then `cute-codex`, then `cutex-codex`, then `codex`.
- `cutex` prints `CLI binary: ...` before launch so you can see which name it resolved.
- The current project directory is mounted into the container at the exact same absolute path.
- The container working directory is that same path.
- Persistent Docker user home is mounted to `/home/<user_name>`.
- Per-profile auth/config/status files are mounted to `/home/<user_name>/.cutex-profiles/<account-id>/`.
- The container runs with the current host user id and group id via `--user <uid:gid>`.
- If an effective proxy exists, `cutex` also injects both uppercase and lowercase proxy env vars.
- For SOCKS proxies, `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` and lowercase variants carry the SOCKS URL so auth and provider clients that only inspect one proxy family still route through the configured proxy.
- `CUTE_CODEX_FORCE_HTTP_TRANSPORT=1` is enabled by default for proxy-configured launches so provider traffic stays on HTTP/SSE instead of the Responses WebSocket path.

## What the image must provide

Minimum requirements:

- The selected CLI name must be available on `PATH`.
- In practice, ship `cute-codex` plus compatibility symlinks for `cutex-codex` and `codex`.
- The image must work in an interactive TTY session (`docker run -it`).
- The image must allow execution as an arbitrary uid/gid passed by `--user`.
- The mounted home at `/home/<user_name>` must be readable and writable.
- The mounted profile directory at `/home/<user_name>/.cutex-profiles` must be readable.

Practical recommendations:

- Include `bash` or another normal shell.
- Include `git`, `ca-certificates`, and common CLI basics.
- Keep the image deterministic and simple; `cutex` does not manage installation inside the container.
- If you expect proxied external providers, include a recent `cute-codex` build with Reqwest SOCKS support enabled.

The image does not need:

- a pre-created Linux user named `<user_name>`
- `tmux` or `zellij`

## Building the bundled runtime image

The repository's `images/cutex-base/Dockerfile` copies a prebuilt Docker-targeted `cute-codex` binary into the runtime image and installs compatibility symlinks at `cutex-codex` and `codex`.

Build it from the repository root:

```sh
docker build -f images/cutex-base/Dockerfile -t cutex-base .
```

## Path model

The workspace path inside the container is intentionally identical to the host path.

Example:

- Host path: `/home/<user_name>/Projects/cutex`
- Container path: `/home/<user_name>/Projects/cutex`

This avoids path drift in tools, session state, and commands that assume the current project path is stable.

## Storage separation

Host-side and Docker-side Codex state are intentionally separated:

- Host runtime Codex home: `~/.cutex/codex-home`
- Docker runtime user home: `~/.cutex/runtime/docker-home`
- Docker runtime Codex home: `~/.cutex/runtime/docker-home/.codex`
- Per-profile auth/config/status files: `~/.cutex/profiles/<account-id>/`

The Docker user home is shared across Docker profiles. The per-profile auth/config/status files are not.

## Proxy notes

- Supported proxy URL schemes are `http`, `https`, `socks5`, `socks5h`, `socks4`, and `socks4a`.
- Prefer `socks5h://...` when you do not want local DNS resolution to leak outside the proxy.
- `cutex proxy set ...` configures a global fallback. `cutex proxy set-profile ...` overrides it for one profile.
- `cutex proxy disable-profile ...` explicitly clears proxy inheritance for one profile.
- `cutex login --name <name>` honors the global proxy only, because there is no profile-specific config before the login snapshot is created.
- While forced HTTP mode is active, `cute-codex` disables Responses-over-WebSocket and rejects realtime conversation startup.

## Host override

A Docker-configured profile can still be forced onto the host for one invocation:

```sh
cutex --host
cutex run <profile> --host
```

This does not rewrite the stored profile runtime.
