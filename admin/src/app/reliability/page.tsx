import { PageHeader } from "@/components/page-header";
import { StatCard } from "@/components/stat-card";
import { EmptyState } from "@/components/empty-state";
import { ErrorPanel } from "@/components/error-panel";
import type { Severity } from "@/components/badges";
import { DataTable, TBody, TH, THead, TR } from "@/components/data-table";
import { ErrorGroupRow } from "@/app/reliability/error-group-row";
import { getErrorsSince } from "@/lib/data";
import { parseErrorSignature } from "@/lib/error-signature";
import { formatNumber } from "@/lib/format";

export const dynamic = "force-dynamic";
export const revalidate = 0;

const DAY_MS = 24 * 60 * 60 * 1000;

/** Volume-based severity for an error group — status lane only. */
function groupSeverity(count: number): Severity {
  if (count >= 100) return "critical";
  if (count >= 20) return "error";
  if (count >= 5) return "warning";
  return "info";
}

export default async function ReliabilityPage() {
  const now = Date.now();
  const since30d = new Date(now - 30 * DAY_MS).toISOString();

  const res = await getErrorsSince(since30d);
  const errors = res.ok ? res.data : [];

  const cutoff24h = now - DAY_MS;
  const cutoff7d = now - 7 * DAY_MS;

  const errors24h = errors.filter(
    (e) => Date.parse(e.created_at) >= cutoff24h
  ).length;
  const errors7dRows = errors.filter(
    (e) => Date.parse(e.created_at) >= cutoff7d
  );
  const affected7d = new Set(
    errors7dRows.map((e) => e.install_id).filter(Boolean)
  ).size;

  type Signature = {
    signature: string;
    count: number;
    installs: number;
    lastSeen: string;
  };
  type Group = {
    kind: string;
    version: string;
    count: number;
    installs: number;
    lastSeen: string;
    latestDetail: string | null;
    signatures: Signature[];
  };

  // kind+version -> signature -> mutable accumulator, so a group's "12 rows"
  // can be broken down by root cause (see error-signature.ts) instead of
  // shown with a single, potentially misleading sample detail.
  type SigAcc = {
    count: number;
    installs: Set<string>;
    lastSeen: string;
  };
  type GroupAcc = {
    kind: string;
    version: string;
    count: number;
    installs: Set<string>;
    lastSeen: string;
    latestDetail: string | null;
    signatures: Map<string, SigAcc>;
  };
  const groups = new Map<string, GroupAcc>();
  for (const e of errors) {
    const version = e.version?.trim() || "unknown";
    const key = `${e.kind} ${version}`;
    let g = groups.get(key);
    if (!g) {
      g = {
        kind: e.kind,
        version,
        count: 0,
        installs: new Set(),
        lastSeen: e.created_at,
        latestDetail: e.detail,
        signatures: new Map(),
      };
      groups.set(key, g);
    }
    g.count += 1;
    if (e.install_id) g.installs.add(e.install_id);
    // Rows arrive oldest-first (see getErrorsSince) — every later row is a
    // newer "last seen" for the group and, when it matches, for its
    // signature bucket too.
    g.lastSeen = e.created_at;
    g.latestDetail = e.detail;

    const signature = parseErrorSignature(e.detail);
    const sig = g.signatures.get(signature);
    if (!sig) {
      g.signatures.set(signature, {
        count: 1,
        installs: new Set(e.install_id ? [e.install_id] : []),
        lastSeen: e.created_at,
      });
    } else {
      sig.count += 1;
      if (e.install_id) sig.installs.add(e.install_id);
      sig.lastSeen = e.created_at;
    }
  }

  const grouped: Group[] = [...groups.values()]
    .map((g) => ({
      kind: g.kind,
      version: g.version,
      count: g.count,
      installs: g.installs.size,
      lastSeen: g.lastSeen,
      latestDetail: g.latestDetail,
      signatures: [...g.signatures.entries()]
        .map(([signature, s]) => ({
          signature,
          count: s.count,
          installs: s.installs.size,
          lastSeen: s.lastSeen,
        }))
        .sort((a, b) => b.count - a.count),
    }))
    .sort((a, b) => b.count - a.count);

  return (
    <div className="space-y-3">
      <PageHeader
        title="Reliability"
        meta={
          res.ok
            ? `${formatNumber(errors.length)} reports / 30d · ${formatNumber(grouped.length)} groups`
            : undefined
        }
      />

      <div className="grid grid-cols-2 gap-2 lg:grid-cols-3">
        <StatCard
          label="Errors 24h"
          value={res.ok ? formatNumber(errors24h) : "—"}
          hint={res.ok ? "reports in the last 24 h" : res.error}
        />
        <StatCard
          label="Errors 7d"
          value={res.ok ? formatNumber(errors7dRows.length) : "—"}
          hint="reports in the last 7 days"
        />
        <StatCard
          label="Affected installs 7d"
          value={res.ok ? formatNumber(affected7d) : "—"}
          hint="distinct installs reporting errors"
        />
      </div>

      {!res.ok ? (
        <ErrorPanel title="Couldn't load errors" message={res.error} />
      ) : grouped.length === 0 ? (
        <EmptyState
          title="No errors in the last 30 days"
          description="Error reports sent by the app will appear here."
        />
      ) : (
        <DataTable>
          <THead>
            <TR>
              <TH className="w-[28px]" />
              <TH className="w-[90px]">Severity</TH>
              <TH className="w-[160px]">Kind</TH>
              <TH className="w-[90px]">Version</TH>
              <TH className="w-[130px] text-right">Rows / installs</TH>
              <TH className="w-[100px] text-right">Last seen</TH>
              <TH>Latest detail</TH>
            </TR>
          </THead>
          <TBody>
            {grouped.map((g) => (
              <ErrorGroupRow
                key={`${g.kind}-${g.version}`}
                group={g}
                severity={groupSeverity(g.count)}
              />
            ))}
          </TBody>
        </DataTable>
      )}
    </div>
  );
}
