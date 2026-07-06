# AGENTS.md — Amperstrand AI Research & Experiments Fork

This is the **Amperstrand experimental fork** of [OpenTollGate/tollgate-rs](https://github.com/OpenTollGate/tollgate-rs).

## ⚠️ AI-Experimental Branch

**The `experimental` branch contains AI-generated code that has NOT been tested on real hardware and is NOT expected to be stable.**

- All code on this branch was primarily written by AI agents (Claude, GPT, GLM) with human review
- 320 unit/integration tests pass against **mock servers** — zero testing against real hardware or real Go v1 routers
- The Spilman channel browser demo (`docs/private/demos/spilman-real/`) runs against a public testnet mint (testnut.cashu.exchange) with cooperative + unilateral close, DLEQ verification on funding, and an interactive utility meter — no persistence
- Documentation (README, WALKTHROUGH, revocation essay) was AI-generated and audited against primary sources — errors may remain
- This branch is for **research and learning only**. Do not rely on it for production use

**Stable code lives on `master`.** The `experimental` branch is where AI-driven exploration happens before human review and merging.

## CRITICAL: Fork-First Stacking Strategy

**This repository is a fork of a public open-source project.** The upstream
project (`OpenTollGate/tollgate-rs`) is maintained by other people. We use a
**fork-first PR stacking** strategy: all work lives on our fork, only the
lowest PR in each stack ever targets upstream, and one at a time.

### Git Remotes

```
origin      → https://github.com/c03rad0r/tollgate-rs.git              (READ-WRITE, primary fork)
amperstrand → https://github.com/Amperstrand/tollgate-rs.git           (READ-WRITE, collaborator fork)
upstream    → https://github.com/OpenTollGate/tollgate-rs.git          (READ-ONLY, fetch only)
ngit        → nostr://npub1.../relay.ngit.dev/tollgate-rs             (READ-WRITE, Nostr git mirror)
```

### Commit & Push Policy (MANDATORY)

1. **Commit early, commit often.** Many small atomic commits, each one logical
   change with a conventional-commit message (`feat:`, `fix:`, `test:`,
   `docs:`, `chore:`, `refactor:`). Do NOT accumulate uncommitted work —
   commit every 10–20 minutes. Uncommitted work is lost work.

2. **Everything gets pushed.** A local commit that isn't pushed is not done.
   Every push goes to BOTH `origin` (GitHub fork) AND `ngit` (Nostr mirror).
   Push after each commit or small group of commits — don't let work pile up
   locally.

3. **Push to ngit on every push to GitHub.** The ngit remote is a mirror.
   After `git push origin <branch>`, also run `git push ngit <branch>` (or
   `--force-with-lease` for rebases). Both remotes must stay in sync.

### Fork-First PR Stacking

```
upstream/master
└── PR A (lowest — targets upstream OpenTollGate/tollgate-rs)
    └── PR B (targets PR A's branch on c03rad0r fork — fork-only)
        └── PR C (targets PR B's branch on c03rad0r fork — fork-only)
```

- **Each PR is one concern.** Small, independently reviewable diffs.
- **Only the lowest PR in a stack targets upstream.** Higher PRs target the
  previous PR's branch on the fork (`c03rad0r/tollgate-rs`). Never have
  multiple PRs from the same stack open upstream simultaneously.
- **When the lowest PR merges upstream**, rebase the next PR onto upstream
  master and retarget it upstream. The stack shifts up.
- **Do NOT open upstream PRs unless explicitly instructed.** When in doubt,
  push to the fork and stack — the project owner decides when to go upstream.

### Upstream Boundary

- `git fetch upstream` — OK (sync from upstream)
- `git push origin <branch>` — OK (work on our fork)
- `git push ngit <branch>` — OK (mirror to Nostr)
- `git push upstream anything` — **FORBIDDEN**
- `gh pr create --repo OpenTollGate/tollgate-rs` — **only for the lowest PR
  in a stack, and only when the project owner approves**

## Project Overview

TollGate v2 — Rust implementation of the TollGate payment protocol for autonomous, device-to-device payment for metered resource delivery. Built on Cashu ecash and Spilman payment channels.

The upstream repo is in the **design phase** — detailed design documents exist under `docs/design/` but no Rust code has been written yet. This fork will contain the implementation work.

### Architecture

```
tollgate-core (lib)           Pure logic, resource-agnostic
    |
    +-- tollgate-net (binary) Network forwarding, feature-flagged per OS
    |     +- Linux / macOS / Windows / OpenWrt
    |     +- FIPS or IP network adapter
    |     +- Cashu wallet
    |
    +-- tollgate-net-esp32 (separate project, not in this repo)
```

### Key Design Documents

Start with `docs/design/core/tollgate-intro.md`, then follow the reading order in `docs/design/README.md`.

## Milestone Plan

| Milestone | Description | Status |
|-----------|-------------|--------|
| M1 | Core Types, Protocol Codec, and Peer State Machine | ✅ Complete |
| M2 | Bootstrap Token Payment, CDK Wallet Integration | ✅ Complete |
| M2.5 | v1 Client Mode (tollgate-rs pays v1 routers) | ✅ Core done, LN invoice client pending |
| M3 | Spilman Payment Channels (server + demo) | 🔄 Server handler done, demo in progress |
| M4 | tollgate-net — IP Peering Deployment | Open |
| M5 | Dynamic Pricing and Operator Controls | Open |
| M6 | FIPS Mesh Integration | Open |
| M7 | Production Hardening and Packaging | Open |

### Current State — Mostly Feature-Complete v1 TollGate

**22.8K lines of Rust** across `tollgate-core` (4K) and `tollgate-net` (10.6K). **320 tests, all passing, 0 ignored.**

Everything runs against mock servers. Not yet tested on real hardware or against a real Go v1 router.

#### v1 Server Mode (replaces Go v1 router) — `tollgate-net v1-server`

All v1 API endpoints implemented with 68 API parity tests:
- `GET /` → Nostr kind 10021 advertisement (pricing, metric, step size)
- `POST /` → Cashu token payment → kind 1022 session event
- `GET /usage` → `"usage/allotment"` text response
- `GET /balance` → JSON balance details
- `GET /whoami` → client MAC address
- `POST /ln-invoice` → Lightning invoice creation (mint quote)
- `GET /ln-invoice` → Invoice status polling (UNPAID → PAID → ISSUED)
- Session lifecycle (create, track, janitor cleanup)
- Profit sharing / payout task
- CORS headers, JSON config file, MAC resolution
- Valve: time-based logging stub (real iptables/nftables is M4)

#### v1 Client Mode (pays upstream Go v1 routers) — `tollgate-net v1-client`

Full Chandler (client) lifecycle:
- Fetch advertisement, parse pricing options
- Select cheapest compatible mint/unit
- Budget validation (max price per ms/byte)
- Cashu token creation → POST payment → session establishment
- Usage polling with auto-renewal at configurable threshold
- Session recovery (re-attach to existing session via `/usage`)
- Token recovery (auto-reclaim via wallet, file fallback)
- Multi-gateway session manager (multiple upstream TollGates)
- Payment throttling (5s minimum between payments)
- Trust policy (allowlist / blocklist / default trust_all/trust_none)
- LN Invoice client payment path — **not yet implemented**
- Auto-detect upstream → session manager wiring — **not yet implemented**

#### Spilman Channel Server — `tollgate-net` with `--features spilman`

Server-side Spilman handler using JSON-in-CBOR bridge pattern aligned with SatsAndSports reference:
- `ChannelLifecycleState` enum (7 states)
- `balance_update_to_payment()` / `channel_close_to_payment()` — CBOR→Payment converters
- `process_balance_update()` via `bridge.process_payment_via_json()`
- `process_channel_close()` via `bridge.execute_cooperative_close_async()`
- Error → ReasonCode mapping (`bridge_error_to_reason`, `close_error_to_reason`)
- 15 inline tests in `server::spilman_handler_tests`
- Network integration tests exist but require live mint (`#[ignore]`)

#### Browser Demo — `docs/private/demos/spilman-real/`

Educational demo with real crypto against testnut.cashu.exchange:
- Waves A-C complete: hand-rolled JS crypto → cdk-wasm bridge → SIG_ALL witness
- Channel open → fund → multi-payment → cooperative close → unilateral close
- E2E lifecycle verified working against live mint (June 2026)
- DLEQ verification: 4/4 proofs verified against testnut (wallet.js)
- Unilateral close: Charlie closes without Alice's cooperation (same swap, validate_due=false per Rust bridge.rs:1681-1689)
- WASM test vectors: 194/194 pass (fixed: 1-year expiry + cache-busting fetch)
- Not yet: persistence, high-level bridge classes
- Phase 0 test vectors: 73/169 pass (fundamental ECDH mismatch @noble/curves vs secp256k1 crate)

### Known Gaps (What's Not Done)

| Gap | Scope | Notes |
|-----|-------|-------|
| Real valve (iptables/nftables) | M4 | Currently time-based logging stub |
| LN Invoice client payment | M2.5 | Server-side LN works; client can't pay via Lightning yet |
| Auto-detect → session manager wiring | M2.5 | `upstream_detector.rs` + `SessionManager` exist, not connected in CLI |
| Physical router testing | All | All 320 tests use mock servers. No real hardware, no real Go v1 interaction |
| CI .ipk artifacts | M4/M7 | GitHub Actions config exists but not producing downloadable OpenWrt packages yet |
| Unilateral / timeout close paths | M3 | ✅ Unilateral close wired into browser demo. Server-side timeout close not yet implemented |
| DLEQ proof verification | M3 Browser | ✅ Implemented — `verify_proof_dleq` wired into funding proof intake, 4/4 proofs verified against testnut |
| Test vectors stale | M3 Browser | ✅ WASM 194/194 pass (fixed: 1-year expiry + cache-busting). Phase 0 crypto.js 73/169 pass (fundamental ECDH mismatch @noble/curves vs secp256k1 crate — documented, not fixable) |

### Key Files

| File | Purpose |
|------|---------|
| `crates/tollgate-net/src/v1/server/handlers.rs` | V1 server API handlers (all endpoints) |
| `crates/tollgate-net/src/v1/server/config.rs` | Server configuration (JSON + CLI) |
| `crates/tollgate-net/src/v1/server/session_store.rs` | Session storage |
| `crates/tollgate-net/src/v1/server/lightning_quotes.rs` | LN invoice quote management |
| `crates/tollgate-net/src/v1/server/payout.rs` | Profit sharing / payout task |
| `crates/tollgate-net/src/v1/server/janitor.rs` | Session cleanup |
| `crates/tollgate-net/src/v1/server/upstream_detector.rs` | Probe upstream TollGates |
| `crates/tollgate-net/src/v1/mod.rs` | V1Client (connect, renew, run loop) |
| `crates/tollgate-net/src/v1/http.rs` | HTTP client for v1 protocol |
| `crates/tollgate-net/src/v1/pricing.rs` | Pricing selection + budget validation |
| `crates/tollgate-net/src/v1/recovery.rs` | Token recovery (auto + file) |
| `crates/tollgate-net/src/v1/session_manager.rs` | Multi-gateway session manager |
| `crates/tollgate-net/src/v1/usage_tracker.rs` | Usage polling + renewal channel |
| `crates/tollgate-net/src/v1/nostr_events.rs` | Nostr event parsing/building |
| `crates/tollgate-net/src/server.rs` | Spilman server handler (~1,460 lines) |
| `crates/tollgate-net/src/main.rs` | CLI entry point (Provider/Client/V1Server/V1Client) |
| `crates/tollgate-core/src/protocol.rs` | CBOR protocol messages |
| `crates/tollgate-core/src/wallet.rs` | Wallet trait |
| `crates/tollgate-net/tests/v1_api_parity.rs` | 68 API parity tests against Go v1 protocol |
| `crates/tollgate-net/tests/v1_e2e_lifecycle.rs` | 7 E2E client lifecycle tests |
| `crates/tollgate-net/tests/v1_server_integration.rs` | 8 server integration tests |
| `crates/tollgate-net/tests/v1_client_integration.rs` | 5 client integration tests |
| `crates/tollgate-net/tests/v1_session_manager_integration.rs` | 3 session manager tests |

### Dependency Graph

```
M1 (Core Types/Codec)
 |
 +--> M2 (Bootstrap Tokens) --> M3 (Spilman Channels) --> M6 (FIPS Mesh)
 |        |                        |
 |        +------------------------+--> M4 (IP Peering Deployment)
 |                                     |
 |                                     +--> M5 (Dynamic Pricing)
 |
 +--> M7 (Production) depends on M4

M2.5 (v1 Client) branches off M2, can proceed in parallel with M3
```

### Critical Path

M1 → M2 → M3 → M4 → M7

### M2 is the First Working TollGate

M2 (bootstrap tokens only, no Spilman) gives a functional TollGate with token-based payments. This is feature-equivalent to v1's payment model but running on v2's architecture. If Spilman channels (M3) prove too hard, M2 + M4 is a shippable product.

### Next Steps

1. **Physical router testing** — flash two OpenWrt routers, one with Go v1, one with tollgate-rs, test client↔server payment flow
2. **Close remaining M2.5 gaps**:
   - **LN Invoice client payment** — `v1/http.rs` needs `POST /ln-invoice` + `GET /ln-invoice` client methods; `v1/mod.rs` needs LN payment branch in `V1Client::connect/renew`. Server-side reference: `v1/server/handlers.rs:418-715`, `lightning_quotes.rs:29-362`
   - **Auto-detect → session manager wiring** — `Crowsnest` (crowsnest.rs) + `SessionManager` (session_manager.rs) + `V1ClientAuto` CLI path (main.rs:445-521) all exist. Missing: platform-specific interface/gateway enumeration (crowsnest only scans caller-supplied `gateway_ips`). Consider merging `V1ClientAuto` into default client path
3. **M3 completion**:
   - **Unilateral close** — Rust/WASM already has `execute_unilateral_close`, `execute_unilateral_close_async`. Browser WASM exposes `WasmSpilmanBridge.executeUnilateralClose(channel_id)`. Missing: wire into demo state machine + UI
   - **DLEQ verification** — WASM exports `verify_proof_dleq(proof_json, mint_pubkey_hex)`. Missing: call it in funding proof intake path
   - **Test vector regeneration** — Phase 0 crypto.js vectors show ECDH mismatch (@noble/curves vs secp256k1 crate); WASM `create_funding_outputs` fails. Need to regenerate vectors from current Rust
   - **Persistence** — cdk-spilman has `ClientStorage` abstraction + `MemoryClientStorage`. Browser needs IndexedDB-backed storage
4. **M4 — Real valve** — iptables/nftables implementation for actual traffic gating

## Working Conventions

### Branching

- Branch from the appropriate base (upstream `master` for the lowest PR in a
  stack, or the previous PR's branch for higher PRs in the stack)
- Branch naming: `m{number}/{short-description}` or `phase-{n}/{description}`
- Push branches to `origin` (c03rad0r fork) AND `ngit` (Nostr mirror)
- Each branch = one PR concern. Stack branches on the fork.

### Implementation Notes

- Design documents in `docs/design/` are the source of truth for protocol behavior
- If implementation reveals issues with the design, document them in `docs/private/` (not in the design docs — those track upstream)
- Architecture Decision Records go in `docs/private/adr/`
- Experiment results, library evaluations, and dead-end notes go in `docs/private/notes/`

### Cashu / Wallet

- M2 uses `cdk` crate for bootstrap token operations (receive, verify, send, balance)
- M3 uses `cdk-spilman` (SatsAndSports fork) compiled to WASM for channel crypto primitives
- Browser demo uses low-level WASM bindings directly (not high-level `WasmSpilmanBridge` classes)
- Strategy documented in `docs/private/adr/0005-native-cashu-ts-spilman-strategy.md`

### Testing

- Every milestone must have integration tests, not just unit tests
- M2 must include a two-node localhost integration test
- M3 must include multi-interval channel lifecycle tests
- M4 must include tests against real network traffic (even if loopback)

## Related Projects

- [tollgate-module-basic-go](https://github.com/OpenTollGate/tollgate-module-basic-go) — TollGate v1 (Go, OpenWrt). The established production implementation. **Read-only reference. Do not interact with this repo.**
- [FIPS](https://github.com/nicobao/fips) — Free Internetworking Peering System. Target mesh network substrate for M6.
- [Cashu](https://cashu.space/) — Ecash protocol used for payments.
