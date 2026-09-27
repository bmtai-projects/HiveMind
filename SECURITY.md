# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub's
[private vulnerability reporting](https://github.com/bmtai-projects/HiveMind/security/advisories/new),
or by email to mukherjee4004@gmail.com.

Please include what you ran, what you observed, and `hivemind --version`. A
proof of concept helps, but a clear description of the mechanism is more
valuable than a working exploit.

You should get an acknowledgement within a few days. This is a small project,
so please be patient rather than assuming silence means indifference — a nudge
after a week is entirely fair.

## Supported versions

Fixes land on the latest release. There are no long-term support branches, so
please upgrade with `hivemind update` before reporting, in case the issue is
already fixed.

## What HiveMind does on your machine, by design

Worth stating plainly, because it shapes what counts as a vulnerability. This
is an agent that:

- **reads and writes files** in its workspace
- **runs shell commands**
- **sends parts of your code** to whichever model provider you have configured

All of that is the product working correctly. The security boundaries we do
intend to hold are:

| Boundary | Intent |
|---|---|
| Workspace confinement | `read_file`, `write_file`, `edit_file`, `list_dir`, `search` resolve paths against the workspace root and reject escapes |
| Shell approval | `run_shell` prompts before executing, unless you passed `--yolo` or are running headless with `-p` |
| Secret-write warning | Content the agent writes via `write_file` or `edit_file` is scanned for credential-shaped strings and flagged back to it |
| Budget enforcement | `--budget` stops at a turn boundary and is not exceeded silently |
| Credential storage | Hosted credentials stay in the local config directory and are never sent anywhere except the auth endpoint |

**A way around any of those is a vulnerability, and we want to hear about it.**
Path-escape bypasses, a shell invocation that skips approval, a budget that is
silently exceeded, or anything that exfiltrates your stored credentials are all
in scope.

One thing the list above deliberately does **not** claim: HiveMind does not
filter what it reads. If you ask it to read a file holding credentials, or it
finds one while searching, those contents go to your configured model provider
like any other file. The scanner runs on writes, not reads. Keep secrets out of
the workspace, or out of the paths you point it at.

## Out of scope

- The agent editing or deleting files inside its own workspace. That is the
  feature.
- `--yolo` or `-p` auto-approving shell commands. Both are documented, opt-in,
  and exist precisely to skip the prompt.
- A model producing wrong, insecure, or destructive code. Review what an agent
  writes before you run it.
- Costs incurred by your own prompts. Use `--budget` if you want a hard cap.
- Reports produced solely by a scanner, with no described impact.
