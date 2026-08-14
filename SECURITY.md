# Security policy and daemon deployment

## Trust boundary

`hearthd` is a performance cache and agent-tool orchestrator for one local OS
user. It is not an authentication, privilege, tenant, sandbox, or workspace
boundary.

Every accepted client has the daemon user's authority exposed by the protocol:
file reads and mutations, arbitrary shell commands and shell selection,
directory traversal, graph indexing, cache control, and shutdown. Unix socket
permissions and peer-UID checks keep out other UIDs; they cannot distinguish a
compromised process running under the same UID.

Therefore:

- never run `hearthd` as root;
- never run it under a UID, entitlement, sandbox profile, environment, or file
  descriptor set more privileged than its clients should receive;
- never expose one daemon as a shared or multi-tenant service;
- do not treat one daemon per UID as a workspace boundary;
- keep the endpoint in an owner-only runtime directory and do not place a
  custom endpoint in a directory writable by another user.

One endpoint per UID is deliberate: it maximizes warm reuse between the user's
workspaces. It also means a client authorized to use that endpoint can operate
on any path the UID can access.

## LLM and agent adapters

Treat all model-generated data as untrusted, including paths, commands,
environment entries, regular expressions, glob lists, edit bodies, graph file
lists, counts, depths, context sizes, timeouts, and retry decisions. A benign
model can still generate extreme values, stale paths, non-idempotent retries,
long commands, or high parallel load.

A production adapter must enforce the authority policy that `hearthd` does not:

1. allowed lexical roots and resolved targets;
2. allowed operations (for example read-only versus mutation versus Bash);
3. environment allowlists and secret redaction;
4. request, output, deadline, and concurrency budgets no larger than Hearth's
   hard limits;
5. human approval for mutations or command execution where the product's risk
   model requires it;
6. no automatic retry after an indeterminate result.

Without such an adapter, giving an LLM access to `hearthd` is intentionally
 equivalent to giving it the daemon OS user's read/write/execute authority.

## Delivery and output semantics

Daemon operations are at-most-once, not exactly-once. The CLI may use an inline
engine only before it connects or when transport proves that no request byte
was sent. If delivery may have begun and the response is lost or malformed, it
returns an `indeterminate` error and does not replay the request.

The FD-passing read path streams directly to the client's descriptor. A
transport failure can therefore leave a valid partial prefix on stdout. Hearth
promises not to duplicate that prefix; it does not promise atomic streamed
output. An adapter that requires all-or-nothing output must spool and validate
before publishing it.

## Filesystem semantics

`followSymlinks: true` deliberately writes through the final symlink to match
`fs.writeFile`-style expectations. It is not an authorization decision. An
adapter enforcing roots must authorize both the caller's lexical path and the
resolved target.

Atomic mode publishes a sibling temporary file by rename and preserves an
existing regular target's mode. In-place mode preserves an existing inode and
its metadata but concurrent readers can observe partial content. Neither mode
currently promises file-and-directory `fsync` crash durability.

## Bash, caches, and processes

`trustCache` is an explicit single-writer optimization. Unrestricted Bash can
modify absolute paths, leave the cwd, or follow symlinks, so a cwd-only cache
invalidation is not sound. A dispatched Bash operation must conservatively
clear filesystem-derived resident state unless a future sandbox enforces a
smaller write set.

Timeout/cancellation/shutdown can terminate and reap process groups Hearth
tracks. POSIX process groups cannot contain a descendant that deliberately
forks, starts a new session, and detaches. Preventing escaped descendants
requires an external OS sandbox or service manager; do not infer that guarantee
from Hearth's timeout alone.

Linux and supported BSDs receive passed descriptors atomically with
`MSG_CMSG_CLOEXEC`. macOS does not provide that flag, so Hearth applies
`FD_CLOEXEC` with `fcntl` immediately after `recvmsg`. A concurrent fork/exec can
race that short window and inherit the descriptor. This residual limitation is
inside the same-UID trust model; use an external sandbox or service manager when
that inheritance boundary must be strict.

## Reporting a vulnerability

Please report suspected vulnerabilities privately to the repository owner
rather than filing a public exploit report. Include the affected commit,
platform, deployment assumptions, minimal reproduction, and whether the client
runs as the same UID. Do not include real credentials, private repository
contents, or unrelated user data.

The detailed source audit and finding ledger live in
[`docs/SECURITY_AUDIT_HEARTHD.md`](docs/SECURITY_AUDIT_HEARTHD.md).
