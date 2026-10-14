# Calod Product Strategy V2 — Refined Plan

Date: 2026-07-23

Status: **decided**. Produced from a structured decision interview with the
founder plus live market research on 2026-07-23. This supersedes
`PRODUCT_STRATEGY_AND_IMPLEMENTATION_PLAN.md` where they conflict. The
engineering audit (Section 3 there) and the effect-ledger architecture
(Sections 5-6 there) remain valid and are referenced, not repeated.

---

## 1. Locked founder constraints (these gate everything)

- **Build capacity:** 10-20 hours/week. Not 25-40. Every timeline below is
  sized to this.
- **Runway:** partially covered by other income. Revenue cannot wait 12+
  months without discomfort, but the founder accepts a long private build
  (see Section 6 and the risk register).
- **Goal:** INR 1-3 lakh/month steady-state profit at **< 10 hours/week**
  of ongoing work. The old INR 5-10 lakh/month-by-month-5 target is
  dropped as a planning basis.
- **Budget:** INR 2-4 lakh, hard ceiling, project spend only (living costs
  excluded).
- **Sales posture:** inbound must be the primary channel; founder-led
  outbound is acceptable but secondary. Few high-paying customers
  (5-15), not volume self-serve.
- **Geography:** India-based founder selling to Indian customers,
  US customers, and a minority of English-speaking EU customers.

## 2. What changed from V1 and why

| Decision | V1 plan | V2 (locked) |
|---|---|---|
| Revenue target | INR 5-10L/mo by month 5 | INR 1-3L/mo steady state, no fixed deadline |
| Business model | OSS + managed cloud | Services-first offer ladder; subscription; small beta cloud (no SLA) later |
| Market | Broad dev-tool ledger, self-serve | Compliance evidence + reliability for teams whose agents touch money/regulated actions |
| Launch gate | Gate 0 interviews, design partners, public beta | **Private until launch**; complete-platform-first (bounded checklist, Section 6) |
| Pricing | Self-serve tiers from INR 2,499/mo | Offer ladder: paid pilot -> deployment -> subscription; dual India/global pricing |
| License | MIT, no restrictions | **BUSL (server) + MIT (SDKs)** |
| Support | < 30 founder-hours/mo | Onboarding calls first 90 days, then async-only |
| Compliance stance | Not addressed | Tooling + published regulation mappings only; no legal claims; founder studies ISO 42001 (course, not accredited cert) |

## 3. Market position (research-grounded)

### 3.1 Positioning statement

> **Calod is the durable evidence and protection layer for AI agents that
> take real-world actions.**
>
> Agents retry. Calod stops a retry from repeating a charge, email, or
> record change — and keeps a tamper-evident, queryable receipt of every
> action, retained and exportable for auditors.

Two hooks, one product:

- **Reliability hook (demand generation):** duplicate charges/emails on
  agent retry. Live, documented pain: CrewAI issue #5802 (May 2026),
  stripe/ai issue #402 (May 2026), an arXiv survey of 12 frameworks
  finding no exactly-once tool boundary, LangGraph resume re-firing tools.
- **Evidence hook (budget):** EU AI Act high-risk obligations enforce
  2026-08-02 (tamper-proof logs retained >= 6 months, human-oversight
  records, audit trails; penalties to EUR 30M / 6%). US pull comes from
  SOC 2 / ISO 42001 procurement screening (72% of enterprise buyers screen
  for ISO 42001; certification engagements run $85k-320k). India pull from
  DPDP and RBI-supervised fintech expectations.

### 3.2 Why the gap exists (competitive map, verified 2026-07-23)

- **Governance platforms** (Credo AI, Holistic AI, OneTrust): policy and
  program management, enterprise custom quotes. None does runtime,
  tamper-evident evidence of agent actions; Credo has runtime governance
  only on its roadmap (May 2026).
- **Observability tools** (Langfuse, LangSmith, Braintrust): traces and
  evals at $29-249/mo. Not compliance-grade evidence stores; traces are
  not receipts.
- **agent-ledger (rune0-dev) and similar libraries:** free, framework-
  agnostic, single-process or bring-your-own-Postgres. They cover basic
  dedup but not multi-worker coordination, fencing, `outcome_unknown`
  recovery, or a standalone retained evidence service.
- **Temporal/Restate/DBOS:** full durable execution. Temporal ($300M
  Series D, native OpenAI Agents SDK integration) absorbs teams willing to
  adopt a workflow engine. Calod explicitly serves teams that will not.
- **Framework-native fixes:** LangGraph/CrewAI will eventually ship
  single-process idempotency. Multi-worker, cross-framework, retained
  evidence remains out of their scope. Time-boxed reassessment trigger:
  if a major framework ships native *durable, multi-worker* tool
  idempotency, reassess positioning within 30 days.

### 3.3 Initial customer profile

- 10-200 person product/fintech/SaaS company, India or US primarily;
- running (or piloting) agents that call payment, email, ticketing, CRM,
  or record-mutation tools;
- Python or TypeScript;
- has had a duplicate-action incident, a near-miss, or an
  auditor/procurement question about what its agents did;
- will not adopt a workflow engine to get protection.

Buyer: CTO / platform lead / AI engineering lead. For the evidence
package, the economic buyer can also be a compliance or risk owner.

## 4. Offer ladder and pricing (locked)

Dual pricing is standard practice (2-4x India/global PPP spread); use INR
for Indian customers, USD elsewhere.

| Step | India | US/Global | What the customer gets |
|---|---:|---:|---|
| **Paid pilot** (2 weeks) | INR 49,000 | $999 | Up to 3 tool calls instrumented with Calod; duplicate-prevention demonstrated on their stack; receipts report. Credited 50% toward deployment if they proceed. |
| **Deployment package** | INR 1.5-3L | $3,000-6,000 | Production self-hosted deployment, retention/redaction config, evidence-export setup, runbook, 30-day support. Scoped from a fixed menu of services the customer picks. |
| **Subscription** | INR 25-40k/mo | $500-800/mo | License (BUSL commercial terms), updates, security patches, async support, evidence-export templates (SOC 2 / ISO 42001 / EU AI Act mappings). |
| **Free** | - | - | MIT SDKs + community server (BUSL, free for internal use). The inbound engine. |

Revenue math for the goal: INR 1-3L/mo = roughly **1 deployment package +
2-3 subscriptions per month**, or **6-8 steady subscriptions**. At 5-15
total customers this is achievable without a sales team.

Pricing rules:
- Anchor pilots against consulting rates (INR 3-8k/hr India; $2-8k audit
  engagements US) — the pilot must look cheap next to those, not next to
  $29/mo SaaS.
- Never discount the subscription below INR 20k/mo (India) / $400/mo
  (global); below that, support economics break.
- Price increases grandfather existing customers for 12 months.

## 5. Product scope at launch (the bounded "complete platform" checklist)

The founder chose complete-platform-first over selling pilots during the
build. To keep that honest, "complete" is this checklist and nothing more.
Anything not listed is post-launch.

**Ledger core (Rust, reusing the current Calod codebase):**
- Effect state machine: claimed -> in_progress -> committed /
  failed_retryable / failed_terminal / outcome_unknown -> reconciled_*
- Atomic acquire with input-digest conflict (409), leases, monotonic
  fencing tokens; stale owners can never commit
- `outcome_unknown` never auto-retries; explicit reconcile API
- Runs + append-only events, per-run sequence, hash chain (tamper-evident)
- Binary-safe segmented WAL with CRC32C, group commit, fail-closed
  recovery (no silent fresh-store fallback), automated backup-restore test
- One configuration source (CLI/file/env), safe bind defaults
- TLS, scoped API keys, per-project tenancy
- Retention policies and compaction; evidence export (JSON/CSV +
  signed manifest) for auditors

**SDKs (MIT):**
- Python: `@calod.effect` decorator + raw client + example adapters for
  Stripe, email, GitHub, **Razorpay/Cashfree (UPI payouts)**, and
  **WhatsApp Business API** (see 5.1)
- TypeScript: middleware at feature parity

**Console:** run timeline, effect state, attempt history, redaction,
manual reconciliation, export. Minimal but complete for those jobs.

**Commercial layer:**
- Accounts/projects/keys, license-key activation for self-hosted
  deployments
- Stripe (global) + Razorpay (India) billing for pilot/deployment/
  subscription
- Small hosted **beta cloud, no SLA**: shared multi-tenant instance for
  evaluation and small accounts, encrypted volumes, daily object-store
  snapshots, tested restore. **India region (Mumbai) first** so Indian
  customers' receipts never leave the country (see 5.1); a second region
  only when US/EU customers ask. No Raft, no multi-region, no uptime
  commitments.

**Docs:** quickstart < 10 minutes, deployment runbook, regulation
mappings page (EU AI Act Art 12/26, SOC 2 CC criteria, ISO 42001 A.9/A.10,
**DPDP Act 2023, RBI outsourcing/audit-trail expectations for regulated
entities** — published as *tooling capabilities mapped to requirements*,
explicitly not legal advice).

**Explicitly NOT in v1:** Raft/multi-node, BYOC/Terraform, SSO/SCIM,
policy/approval workflow builder, MCP reverse proxy, vector/semantic
memory, more languages, general Redis parity, public managed GA with SLA.

### Engineering reuse (from V1 audit, still valid)

Reuse: RESP parser, sharding, binary-safe DataTypes, streams/consumer
concepts, Pub/Sub, Docker + benchmark/stress harnesses. Replace:
stringified AOF (-> versioned binary WAL), silent recovery fallback,
config overlay chaos, canned observability replies. Remove: graph, RDMA,
tiered memory. Freeze: broad Redis command parity. Do the Cargo-workspace split
mechanically, without semantic changes.

### 5.1 India-market appeal pack

Features and services tuned for Indian enterprises, businesses, and
startups. Scope rule: v1 adds only adapters, content, and deployment
options — nothing that grows the ledger core.

**Features (in v1):**

1. **Razorpay/Cashfree adapters (UPI payouts, payment links, refunds).**
   Indian agents move money through Razorpay, not Stripe. UPI's
   "debited but status pending" failure mode is the Indian duplicate-
   charge story; the adapter demo reproduces it and shows Calod's
   `outcome_unknown` reconciliation resolving it safely.
2. **WhatsApp Business API adapter.** The Indian duplicate-email problem
   is actually a duplicate-WhatsApp problem (order confirmations,
   appointment reminders, payment links sent twice to customers).
   High-visibility demo; every Indian B2C business runs on WhatsApp.
3. **India data residency.** Beta cloud in an Indian region (Mumbai);
   self-hosted deployments default to in-country object storage. Sales
   line: "your agents' receipts never leave India" — directly relevant to
   DPDP Act 2023 duties and RBI's payment-data localization expectations
   for regulated fintechs.
4. **DPDP + RBI evidence mappings** on the regulation-mappings page
   (alongside EU AI Act / SOC 2 / ISO 42001), so an Indian fintech's
   compliance lead can see which audit-trail expectations the receipts
   cover.
5. **GST-compliant invoicing and INR billing** via Razorpay (already in
   the commercial layer) — Indian enterprises cannot expense a foreign
   card charge cleanly; GST invoices and INR POs remove the biggest
   procurement friction.

**Services (menu items in the deployment package, no new product):**

6. **On-prem / customer-VPC deployment tier** (deployment package +
   INR 50k-1L): many Indian enterprises and NBFCs will not send
   action receipts to a third-party cloud at any price; a runbook-driven
   on-prem install converts them. Air-gapped variant is post-launch,
   enterprise-only, priced per engagement.
7. **Half-day "agent reliability audit" workshop** (INR 40-60k standalone
   or bundled): remote walkthrough of the customer's agent stack hunting
   duplicate-action and evidence gaps, delivered as a written findings
   report. Doubles as the pilot's sales asset and requires no code.
8. **Startup program:** DPIIT-recognized startups (and portfolio
   companies of major Indian funds) get the pilot at INR 25k and 3 months
   of community-tier subscription free after deployment. This seeds case
   studies and referrals in the exact segment that talks the most.
9. **IST-hours onboarding with WhatsApp/Slack support channel** during
   the 90-day onboarding window. Indian business buyers expect
   WhatsApp-level responsiveness; limiting it to the onboarding window
   keeps it inside the support-time cap.

**Deliberately excluded** (would break the low-ops model): Aadhaar/
DigiLocker integrations, GST e-invoicing for the customer's own business,
per-customer custom compliance reports, 24x7 support commitments.

## 6. Roadmap at 10-20 hours/week

Estimated from the V1 milestone sizes, roughly doubled for real hours.
Calendar dates assume a 2026-08-01 start; shift as needed.

| Phase | Duration | Contents | Exit test |
|---|---|---|---|
| P0 Hygiene | wks 1-4 | Workspace split, CI (fmt/clippy/tests/OpenAPI lint), fmt fix, README/story cleanup, remove false claims | CI green on all platforms |
| P1 Trustworthy core | wks 5-12 | Config unification, binary WAL + crash recovery, fail-closed startup, TLS + API keys, backup-restore automation | 10k randomized kill -9 transitions pass; restore verified daily |
| P2 Ledger MVP | wks 13-22 | Effect state machine, fencing, conflicts, outcome_unknown + reconcile, runs/events/hash chain, REST /v1, Python SDK, Stripe adapter | 100 workers race -> one owner; 100 retries replay one receipt; stale fence rejected; 409 on digest mismatch |
| P3 Breadth | wks 23-32 | TypeScript SDK, LangGraph/CrewAI/OpenAI-Agents + MCP-middleware examples, Razorpay/Cashfree + WhatsApp Business adapters (5.1), console, OTel/Prometheus, retention + evidence export | Fresh user protects first effect in < 10 min; auditor export opens in a spreadsheet |
| P4 Commercial | wks 33-42 | Accounts/keys, license activation, Stripe + Razorpay billing, quotas, beta cloud (no SLA) with snapshots + restore drills, legal docs (ToS, privacy, DPA template, BUSL text, trademark policy) | End-to-end: signup -> pay -> deploy -> protected effect -> export, with no founder touch |
| **Launch** | ~wk 42-46 | Public repo, bug-repro content series, HN/framework-forum posts, outbound list of 100 target companies | First paid pilot within 30 days of launch |

**Total: ~10-11 months to launch.** First revenue target: within 30 days
of launch (paid pilot). Steady INR 1-3L/mo: realistically months 3-9
after launch.

### Risk register for the private-build choice

The founder accepted a private build with no design partners. The
mitigations that do not violate that choice:

1. **Time-box:** if P2 exit (week ~22) is not met by week 30, stop and
   re-scope before spending more.
2. **Kill criteria reviewed monthly** from launch: < 2 paid pilots in
   first 90 days post-launch, or pilots that never convert to deployment,
   = reposition or stop.
3. **Content before code where possible:** the bug-reproduction articles
   (Section 7) are written during P1-P3 and double as demand tests —
   if the CrewAI/Stripe reproduction posts get no engagement, that is an
   early warning signal obtainable without showing the product.
4. Budget is released per phase (Section 8), never up front.

## 7. Go-to-market (inbound-primary)

**Primary: executable bug-reproduction content.** One article per
failure mode, each reproducing a real, cited failure and showing the
10-line Calod fix:

1. "CrewAI retried the task and charged twice" (issue #5802)
2. "LangGraph resume re-fired the tool" (documented checkpoint behavior)
3. "Stripe's own agent SDK issues a new idempotency key per agent retry"
   (stripe/ai #402)
4. "What your agent did, proved: receipts for SOC 2 / ISO 42001 / EU AI
   Act"
5. "UPI said 'pending', the agent paid again" — Razorpay/UPI
   `outcome_unknown` reproduction for the Indian audience
6. "Your agent sent the customer the same WhatsApp message twice"
   (WhatsApp Business API duplicate-send reproduction)
7. TypeScript variant of #1-#3

Published on the Calod site + cross-posted (dev.to, framework forums,
relevant subreddits). These compound in search and require no ongoing
spend. Cadence: 1 per 2-3 weeks starting in P1 — they are also the
private-build demand test (Section 6, mitigation 3).

**Secondary: open-source presence.** MIT SDKs with excellent READMEs,
interactive demo, honest compatibility/guarantee matrix. HN + framework
community launches at release. BUSL keeps a competitor from hosting the
server against us; SDKs stay frictionless.

**Tertiary: founder-led outbound, capped.** A list of ~100 companies
(Indian fintech SaaS, US YC-style startups shipping agents, EU
English-speaking SaaS in high-risk categories). Max 10 personalized
messages/week, only after launch, referencing the relevant bug-repro
article. This is a conversion assist, not the engine.

**Later channel (post-first-3-customers):** ISO 42001 / AI Act compliance
consultants get free tooling licenses and refer clients needing the
evidence layer. Do not invest here before having case studies.

**Compliance credibility posture:** tooling + published mappings only;
no legal claims, no "certified" language. Founder takes an ISO 42001
course for fluency (e.g. a Udemy course is fine for learning; it is NOT
an accredited credential and must not be marketed as one). Partner with
a compliance consultant later if evidence-package deals demand it.

## 8. Budget (gated, INR 2-4L hard ceiling)

| Item | Min | Max | Release condition |
|---|---:|---:|---|
| Content hosting, domain, demo infra | 10k | 20k | Immediate |
| Independent Rust/storage/security review | 50k | 80k | P2 exit |
| Console/docs design contractor | 25k | 50k | P3 start |
| Cloud + monitoring + backups (to launch + 6 mo) | 35k | 70k | P4 start |
| Legal: BUSL text review, ToS/privacy/DPA, trademark | 25k | 45k | P4 start |
| Launch content sponsorship/promotion | 25k | 60k | Launch |
| ISO 42001 course + misc founder upskilling | 5k | 10k | Any time |
| Contingency/incident reserve | 25k | 65k | Hold, do not pre-spend |
| **Total** | **2.0L** | **4.0L** | |

Do not spend on: GPU hardware, generic ads, Kubernetes, Redis command
parity, accredited certification (until revenue justifies it).

## 9. Operating model after launch

Target steady state: **< 10 hours/week**, per the locked goal.

- Support: onboarding calls (scheduled) for each customer's first 90
  days, then async-only (GitHub issues + email, 48h response target).
  Hard cap 15 h/month; if support repeatedly exceeds the cap, raise
  prices or narrow the customer profile.
- Reliability: beta cloud carries no SLA; status page + daily restore
  test + alerting only. Self-hosted customers run their own infra —
  the runbook and backup-restore tooling are the product's answer.
- At INR 3L/mo sustained: hire a part-time support contractor before
  adding any new product surface.

## 10. Decision dashboard and kill rules

Monthly review from launch:

| Metric | Target |
|---|---:|
| Paid pilots in first 90 days post-launch | >= 2 |
| Pilot -> deployment conversion | >= 40% |
| Subscription gross margin | >= 75% |
| Monthly churn (subscriptions > 2 mo old) | < 5% |
| Founder support time | < 15 h/month |
| Time to first protected effect (new user) | < 10 min |
| Bug-repro article engagement (pre-launch signal) | meaningful discussion/reshares per post |

Kill or reposition if any persists for two review periods:
- fewer than 2 paid pilots in the first 90 days after launch;
- pilots convert but nobody renews a subscription;
- a major framework ships durable multi-worker idempotency natively
  (30-day reassessment clock);
- support load structurally exceeds side-hustle capacity;
- every serious prospect demands full workflow orchestration.

## 11. Immediate next actions

1. Fix `cargo fmt`, clean the README/public claims (remove "unmatched
   speed", "horizontal scalability", "production stability", "Redis
   drop-in", "exactly once") — the repo is the future inbound surface.
2. Draft the BUSL + MIT license split and the trademark policy skeleton.
3. Write bug-repro article #1 (CrewAI #5802) with a runnable
   reproduction — no Calod mention needed yet; it seeds the search
   channel and tests demand.
4. Start P0 (workspace split + CI).
5. Put the P2 time-box (week 30) and this document's kill rules on a
   recurring monthly review.

## 12. Research sources (verified 2026-07-23)

Problem evidence:
- [arXiv: tool-call side-effect issues across 12 agent frameworks](https://arxiv.org/pdf/2603.20625)
- [CrewAI issue #5802 — duplicate tool execution on retry](https://github.com/crewAIInc/crewAI/issues/5802)
- [stripe/ai issue #402 — agent-level retry creates duplicate charges](https://github.com/stripe/ai/issues/402)
- [tianpan.co — idempotency problem in agentic tool calling](https://tianpan.co/blog/2026-04-19-idempotency-agentic-tool-calling-saga-deduplication)

Competition:
- [rune0-dev/agent-ledger](https://github.com/rune0-dev/agent-ledger)
- [Temporal/durable-execution landscape 2026](https://zylos.ai/research/2026-02-17-durable-execution-ai-agents/)
- [Credo AI vs Holistic AI — runtime governance gap](https://kosmoy.com/resources/blog/credo-ai-vs-holistic-ai/)

Compliance forcing functions:
- [EU AI Act deployer obligations, Art 26](https://fronterio.com/en/blog/eu-ai-act-complete-guide-2026)
- [EU AI Act high-risk enforcement timeline/penalties](https://runtimeai.io/blog/2026-05-10-eu-ai-act-compliance.html)
- [ISO 42001 engagement costs and procurement screening](https://elevateconsult.com/insights/iso-42001-certification-cost-breakdown-what-enterprise-ai-teams-pay-in-2026/)
- [ISO 42001 consultant market immaturity](https://certbetter.com/blog/iso-42001-cost-what-ai-certification-actually-costs-in-2026/)

Pricing:
- [India vs international SaaS pricing](https://www.playto.so/blogs/how-to-price-your-saas-for-indian-vs-international-customers-in-2026)
- [PPP pricing baseline](https://zylos.ai/research/2026-02-14-saas-pricing-strategy/)
- [Duplicate-payment losses in AP](https://beancount.io/blog/2026/05/11/accounts-payable-automation-2026-ai-invoice-capture-three-way-match-touchless-approvals-cut-costs-eliminate-duplicate-payments-guide)
