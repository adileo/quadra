---
layout: home
title: Compliance
description: The evidence auditors ask for, built into the platform. How Quadra supports internal controls, tax record rules, sustainability reporting and traceability, and what is coming next.
pageClass: q-v2 q-safe-page
---

<div class="q-page-intro">

<div class="q-eyebrow">Compliance</div>

# Built for the audit

<p class="q-lead">Audits ask the same questions everywhere: who changed this, when, why, and who approved it? Can the records be altered? Does everyone compute this number the same way? In Quadra the answers are not a report someone builds later. They are how the platform stores and checks everything.</p>

<p class="q-note">No software is compliant on its own: companies are certified, and in some fields so are the applications they use. Quadra gives you the controls and the evidence those certifications ask for. Requirements differ by country and by sector, so check them with your auditor.</p>

</div>

<div class="q-section">

<div class="q-eyebrow">What auditors ask for</div>

## The requirement, and what covers it

<div class="q-table">

| Requirement | In Quadra | |
|---|---|---|
| Audit trail: who, when, what, why | Every value ever written is kept with its user, time and cause. `why()` and the history API explain any record. | <span class="q-avail">Available</span> |
| Records that can't be altered | The history is append-only and chained by SHA-256 hashes. `quadra db verify` detects any change to the past. | <span class="q-avail">Available</span> |
| Final documents stay final | `frozen when` makes a confirmed document read-only. Corrections go through actions, and leave a trace. | <span class="q-avail">Available</span> |
| Segregation of duties | `role` and `deny` rules, for example: whoever creates an order can't approve it. Bulk changes can require the approval of another user. | <span class="q-avail">Available</span> |
| Access control | Permissions on records and fields, applied in the app, the API and every action. API tokens can be read-only and expire. | <span class="q-avail">Available</span> |
| Change management | Code in git, checked by `quadra check`, tested, previewed on the real data and deployed. Migrations run in one transaction or not at all. | <span class="q-avail">Available</span> |
| Consistent KPI definitions | Certified metrics: a change of meaning, anywhere in the model, stops the build until it is approved. | <span class="q-avail">Available</span> |
| Traceability of goods | Append-only ledgers keyed by product, location and lot: one step back, one step forward. | <span class="q-avail">Available</span> |
| Proof of who wrote the history | Every commit signed with an Ed25519 key kept outside the database; `quadra db verify` checks every signature. | <span class="q-avail">Available</span> |
| Trusted timestamps | The head of the chain sealed by an external timestamp authority. | <span class="q-soon">Coming soon</span> |
| Right to erasure (GDPR) | `erase` empties a person's data everywhere (tables, history, log, archives, files), while the history stays verifiable. Data the law requires you to keep is protected by `retain`. | <span class="q-avail">Available</span> |
| Data retention | `retain` and `expire` on entities and fields: kept as long as the law asks, erased when it's no longer needed. A person's data can be exported (GDPR art. 15 and 20). | <span class="q-avail">Available</span> |
| Electronic signatures | Signatures with a meaning (approved, reviewed), bound to the record and to the history. | <span class="q-soon">Coming soon</span> |

</div>

</div>

<div class="q-section">

<div class="q-eyebrow">By field</div>

## Where it helps most

<div class="q-cards q-cards--3">
  <div class="q-card">
    <h4>Internal controls</h4>
    <p class="q-std">SOX · L. 262/2005 · D.Lgs. 231/2001</p>
    <p>Segregation of duties, change management, an audit trail and stable KPI definitions: the controls over financial reporting that auditors test every year.</p>
  </div>
  <div class="q-card">
    <h4>Tax records</h4>
    <p class="q-std">Verifactu · NF525 · GoBD</p>
    <p>Rules that require chained, unalterable invoice and accounting records. That is how Quadra stores everything; the formats and submissions of each country are built as an app on top.</p>
  </div>
  <div class="q-card">
    <h4>Sustainability reporting</h4>
    <p class="q-std">CSRD · ESRS</p>
    <p>Reports that need external assurance. Certified metrics fix each indicator’s definition, and <code>why()</code> shows where every number came from.</p>
  </div>
  <div class="q-card">
    <h4>Traceability</h4>
    <p class="q-std">Reg. (EC) 178/2002 · ISO 22005 · ISO 13485 · IATF 16949</p>
    <p>Food, medical devices, automotive: every lot in, every lot out, kept in ledgers that are never edited.</p>
  </div>
  <div class="q-card">
    <h4>Life sciences <span class="q-soon">Partly coming soon</span></h4>
    <p class="q-std">FDA 21 CFR Part 11 · EU GMP Annex 11</p>
    <p>The audit trail is there today. Electronic signatures and a validation package are on the roadmap.</p>
  </div>
  <div class="q-card">
    <h4>Information security</h4>
    <p class="q-std">ISO/IEC 27001 · SOC 2</p>
    <p>Logging, access control, segregation of duties and change management are built in: evidence for the controls of your management system.</p>
  </div>
</div>

</div>

<div class="q-section">

<div class="q-eyebrow">On the roadmap</div>

## Coming soon

<p class="q-lead">The next steps make the history provable to an outsider at a point in time, and give records signatures with a meaning.</p>

<div class="q-cards q-cards--2">
  <div class="q-card">
    <h4>Trusted timestamps <span class="q-soon">Coming soon</span></h4>
    <p>The head of the chain sealed at regular intervals by an external timestamp authority (RFC 3161, eIDAS qualified where required): proof that the history existed at that time.</p>
  </div>
  <div class="q-card">
    <h4>Electronic signatures <span class="q-soon">Coming soon</span></h4>
    <p>Sign a record with a meaning, after signing in again: the name, the time and the meaning are bound to that record and written into the history.</p>
  </div>
</div>

</div>

<div class="q-section q-page-cta">

## See it for yourself

<p class="q-lead">See how the controls work on a real model, or read how the history is stored.</p>

<a class="q-more" href="/safe-by-design">Why it’s safe by design <span aria-hidden="true">→</span></a>
&nbsp;
<a class="q-more" href="/reference/storage">How the history is stored <span aria-hidden="true">→</span></a>

</div>
