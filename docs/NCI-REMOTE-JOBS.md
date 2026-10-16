# NCI remote jobs (Gadi PBS)

Agents can submit batch jobs to NCI Gadi (PBS Pro) and get **one completion message** when the job
ends, without polling. cutex learns the job's state from Gadi's own scheduler (`qstat`) over a
dedicated, restricted SSH key; PBS notification mail is an optional trigger that makes cutex look
sooner. Mail alone never completes a job.

Linux hosts only (the Job Service and the verifier exist only there). Everything is off until you
set it up; an unconfigured host makes no SSH connection and no mail connection.

## What you need

| | |
|---|---|
| cutex | a Linux build advertising `nci-verification`, `nci-host-keygen` and `inbound-mail` (`GET /v2/capabilities`) |
| Job Service | the optional component: `cutex component install job-service` (or the desktop's **Install Job Service**). It runs with the NCI source; `job-source-nci` then appears in the capabilities |
| Gadi account | your NCI username, a project with SU, and ordinary SSH access to `gadi.nci.org.au` for the agent that submits (see "How agents submit") |
| Gadi host keys | the lines `ssh-keyscan gadi.nci.org.au` prints, **checked** against the fingerprints NCI publishes (NCI user documentation or the NCI help desk) |
| Mailbox (optional) | a dedicated mailbox with IMAP over TLS (port 993) and an app password, that receives PBS mail **directly** from `pbs.gadi@nci.org.au` (no forwarding: it breaks the DKIM signature cutex checks). Gmail with 2-step verification and an app password works. OAuth-only mailboxes (e.g. most Microsoft 365 / university mailboxes) cannot be used. Without a mailbox cutex checks every 30 min – 7 days (default 6 h) |

## Set up (desktop: Settings → Tools → NCI remote jobs)

The desktop page walks through the same steps; each maps to an owner-only Management v2 route.

1. **Generate the key on the host** – `POST /v2/nci-verification/key` `{}`. cutex creates an
   ed25519 key without passphrase (comment `cutex-qstat`) under `~/.cutex/runtime/nci-verification`
   (mode 0600). The private half is never returned. The status shows `publicKey`,
   `publicKeyFingerprint` and `authorizedKeysTemplate`.
2. **On Gadi** (in your own SSH session):
   - `echo $HOME` – your Gadi home, e.g. `/home/123/ab1234`.
   - Save the helper from `GET /v2/nci-verification/helper` (`script`, check its `sha256`) as
     `~/.cutex/nci-qstat-only.py`, then `chmod 700 ~/.cutex ~/.cutex/nci-qstat-only.py`.
   - Append the `authorizedKeysTemplate` line to `~/.ssh/authorized_keys`, with `<GADI_HOME>`
     replaced by your home:

     ```text
     restrict,command="/usr/bin/python3 -I /home/123/ab1234/.cutex/nci-qstat-only.py" ssh-ed25519 AAAA… cutex-qstat
     ```

     `restrict` turns off forwarding, PTY and rc files; the helper runs only
     `/opt/pbs/default/bin/qstat -xf -F json <job id>` and refuses everything else (exit 126).
     So this key can read job status and nothing more.
3. **Gadi host keys, username, enable** – `PUT /v2/nci-verification/config`:

   ```json
   { "username": "ab1234", "hostKeys": ["ssh-ed25519 AAAA…", "ssh-rsa AAAA…"],
     "hostKeyConfirmed": true, "useGeneratedKey": true, "enabled": true,
     "pollIntervalSeconds": 21600 }
   ```

   Pin every confirmed key (Gadi's name rotates over several login nodes). There is no
   trust-on-first-use; a changed host key fails as `qstat_host_key_mismatch`.
4. **Mailbox (optional)** – `PUT /v2/inbound-mail/config`
   `{enabled, server, port, username, password, mailbox?, sources: ["nci"], address?}`, then
   `POST /v2/inbound-mail/test`. `address` is the address PBS mails (`qsub -M`); omitted means
   `username` when that is an address. Add a "never spam" rule for `pbs.gadi@nci.org.au`.
5. **Check** – the first registered job is queried at once. `GET /v2/nci-verification` shows
   `lastQueryAt`, `lastSuccessAt` and `errorCode`.

To stop: `DELETE /v2/nci-verification/config` (removes the stored key; remove the line from Gadi's
`authorized_keys` too) and `DELETE /v2/inbound-mail/config`.

## How agents submit

The `nci-gadi` skill tells the agent the exact steps. In short, through the Job Service tools
(`cutex_job` MCP server, Codex agents):

1. `prepare_remote {"source":"nci"}` → `pbsArgs` (`-N cx<token> -m abe`, plus `-M <address>` when a
   mailbox is set up) and a `correlationToken`.
2. `qsub <pbsArgs…> <resources> job.pbs` **on Gadi, with your ordinary SSH access** – the agent's
   shell needs network access and a login that can run `qsub` (your own key / agent). The dedicated
   `cutex-qstat` key cannot submit anything.
3. `register_remote {source, correlationToken, remoteId: "<id>.gadi-pbs", …}`.

cutex then queries `qstat` (at most one SSH connection per 180 s for the whole host, ≥ 180 s per job;
a PBS end mail moves the next check forward) and delivers `exited` / `failed` (`nonzero_exit`,
`walltime_exceeded`, `aborted`) / `cancelled` once. The desktop's Jobs page lists the job with its
PBS id; **Mark result…** settles a job by hand (e.g. after its PBS history expired).

## Status codes (`errorCode` in `GET /v2/nci-verification`)

| Code | Meaning / what to do |
|---|---|
| `qstat_ssh_missing` | no `/usr/bin/ssh` on the host: install the OpenSSH client |
| `qstat_host_key_mismatch` | Gadi presented a key you did not pin: re-check NCI's fingerprints, update the host keys |
| `qstat_ssh_auth_failed` | Gadi refused the key: check the `authorized_keys` line (one line, exact key) |
| `qstat_ssh_connect_failed` | network / DNS / timeout reaching Gadi; retried with backoff |
| `qstat_ssh_failed` | another ssh failure |
| `qstat_helper_missing` | the `command="…"` path does not name the helper: check the home path and file |
| `qstat_helper_refused` | the helper refused the command: reinstall the helper cutex serves |
| `qstat_job_unknown` | PBS has no record of the job (wrong id or history expired): mark the result by hand |
| `qstat_remote_failed` | qstat failed on Gadi for another reason |
| `qstat_owner_mismatch` / `qstat_name_mismatch` / `qstat_project_mismatch` | the job PBS returned is not the registered one (another user, renamed job, other project) |
| `job_service_unavailable` / `job_service_source_disabled` | the Job Service is not running / not started with the NCI source |

Failures retry after 5 minutes, doubling up to 1 hour. ssh's error text is only classified, never
stored or shown.

## Limits

- Unattended login: the key has no passphrase; if NCI's policy requires MFA for SSH keys, leave this
  off and mark results by hand.
- How long Gadi keeps finished jobs in `qstat -x` history is not established; a job checked after its
  history expired stays open (`qstat_job_unknown`) until marked by hand.
- One Gadi username per cutex host (the owner's).
- Claude runtime agents do not get the `cutex_job` tools yet; use a Codex agent for Gadi jobs.
