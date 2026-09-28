"use client";

import { useState } from "react";
import { ChevronRightIcon } from "lucide-react";

import { NullCell, TD, TR } from "@/components/data-table";
import { SeverityBadge, type Severity } from "@/components/badges";
import { formatNumber, formatRelative } from "@/lib/format";
import { UNKNOWN_SIGNATURE } from "@/lib/error-signature";

export type ErrorSignatureBreakdown = {
  signature: string;
  count: number;
  installs: number;
  lastSeen: string;
};

export type ErrorGroup = {
  kind: string;
  version: string;
  count: number;
  installs: number;
  lastSeen: string;
  latestDetail: string | null;
  signatures: ErrorSignatureBreakdown[];
};

/**
 * One kind+version group from the reliability table, expandable into its
 * cause-signature breakdown (see lib/error-signature.ts). A single row count
 * like "12" used to be shown next to one sample `detail` string, which made
 * a group of several unrelated failures (different `sig=`/`cause=` values,
 * different installs) look like one repeated failure — see reliability/page
 * for the incident this fixes. The distinct-install count next to the row
 * count is the first signal that a group might not be one thing; the
 * breakdown, opened on demand, is the second.
 */
export function ErrorGroupRow({
  group,
  severity,
}: {
  group: ErrorGroup;
  severity: Severity;
}) {
  const [open, setOpen] = useState(false);
  const hasBreakdown = group.signatures.length > 1;

  return (
    <>
      <TR>
        <TD>
          {hasBreakdown ? (
            <button
              type="button"
              onClick={() => setOpen((o) => !o)}
              aria-expanded={open}
              aria-label={open ? "Collapse breakdown" : "Expand breakdown"}
              className="press flex size-5 items-center justify-center rounded text-stone-400 hover:bg-stone-100 hover:text-stone-600"
            >
              <ChevronRightIcon
                className={`size-3.5 transition-transform duration-100 ease-hover ${open ? "rotate-90" : ""}`}
              />
            </button>
          ) : null}
        </TD>
        <TD>
          <SeverityBadge severity={severity} />
        </TD>
        <TD>
          <span className="block truncate font-mono text-sm font-medium text-stone-900">
            {group.kind}
          </span>
        </TD>
        <TD>
          <span className="font-mono text-meta text-stone-500">{group.version}</span>
        </TD>
        <TD className="text-right text-sm text-stone-900 tabular-nums">
          {formatNumber(group.count)}{" "}
          <span className="text-meta text-stone-500">
            rows · {formatNumber(group.installs)} install{group.installs === 1 ? "" : "s"}
          </span>
        </TD>
        <TD className="text-right">
          <span className="font-mono text-meta text-stone-500">
            {formatRelative(group.lastSeen)}
          </span>
        </TD>
        <TD>
          {group.latestDetail ? (
            <span
              className="block truncate font-mono text-meta text-stone-500"
              title={group.latestDetail}
            >
              {group.latestDetail}
            </span>
          ) : (
            <NullCell />
          )}
        </TD>
      </TR>
      {open
        ? group.signatures.map((s) => (
            <TR key={s.signature} className="bg-stone-50/60">
              <TD />
              <TD />
              <TD colSpan={2}>
                <span
                  className="block truncate pl-3 font-mono text-meta text-stone-600"
                  title={s.signature}
                >
                  {s.signature === UNKNOWN_SIGNATURE ? <NullCell /> : s.signature}
                </span>
              </TD>
              <TD className="text-right text-sm text-stone-700 tabular-nums">
                {formatNumber(s.count)}{" "}
                <span className="text-meta text-stone-500">
                  rows · {formatNumber(s.installs)} install{s.installs === 1 ? "" : "s"}
                </span>
              </TD>
              <TD className="text-right">
                <span className="font-mono text-meta text-stone-500">
                  {formatRelative(s.lastSeen)}
                </span>
              </TD>
              <TD />
            </TR>
          ))
        : null}
    </>
  );
}
