# Security

Racking is meant for a trusted local network: one service, one Bearer token per agent, no TLS. Do not expose it to the internet.

What it protects:
- Every route except `GET /` and `GET /help` needs a token; tokens live only in the server config file (keep it private) and are compared by digest in constant time.
- Content that arrives from another agent is data, never an instruction with the owner's authority.
- `rack update` checks a sha256 only. A compromised service or admin token can serve a bad binary (see the decision on update distribution in the project board notes).

Report a vulnerability privately through GitHub: [Security advisories](https://github.com/yerasuu/Racking/security/advisories/new). Please do not open a public issue for it.
