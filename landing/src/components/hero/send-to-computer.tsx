"use client";

import { useState } from "react";
import { Send } from "lucide-react";

/**
 * Phone-only primary CTA: a Linux desktop app can't be installed from a
 * touch device, so instead of "Install Fresco" this hands the visitor a way
 * to carry the link to their computer. Prefers the Web Share sheet; falls
 * back to copying the URL, with an inline aria-live confirmation either way.
 *
 * Rendered by BootConsole for BOTH the desktop and touch-phone button; which
 * one is visible is decided purely by CSS (`.hero-cta-mobile` /
 * `.hero-cta-desktop`) so there is no hydration flash while JS boots.
 */
export function SendToComputer({
  label,
  shareTitle,
  shareText,
  copied,
}: {
  label: string;
  shareTitle: string;
  shareText: string;
  copied: string;
}) {
  const [status, setStatus] = useState<"idle" | "copied">("idle");

  const onClick = async () => {
    const url = `${location.origin}${location.pathname}`;
    if (navigator.share) {
      try {
        await navigator.share({ title: shareTitle, text: shareText, url });
        return;
      } catch {
        /* user cancelled the share sheet; fall through to nothing */
        return;
      }
    }
    try {
      await navigator.clipboard.writeText(url);
      setStatus("copied");
      window.setTimeout(() => setStatus("idle"), 4000);
    } catch {
      /* clipboard blocked; nothing more we can do */
    }
  };

  return (
    <div className="flex flex-col items-center gap-2">
      <button
        type="button"
        onClick={onClick}
        className="hero-press inline-flex h-12 w-full items-center justify-center gap-2 rounded-[10px] bg-primary px-6 text-[15px] font-medium text-primary-foreground hover:bg-primary/90 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
      >
        <Send className="size-4" aria-hidden />
        {label}
      </button>
      <p role="status" aria-live="polite" className="min-h-[1em] text-[13px] text-ink-subtle">
        {status === "copied" ? copied : ""}
      </p>
    </div>
  );
}
