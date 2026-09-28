"use client";

import { useEffect, useRef } from "react";

/** public/hero-poster.* was extracted at this time (ffmpeg -ss 5). */
/* The loop was trimmed to start 3s into the source clip (its opening is a
   near-white frame). Within that trimmed file, 5s lands on a sharp,
   well-lit, eyes-open frame — far more "clearly playing" than the pale,
   half-turned opening — so both the poster and the reduced-motion park
   point use it. */
const POSTER_TIME = 5;

/**
 * The demo wallpaper, looping inside the hero's desktop-window frame.
 *
 * Policy: muted + playsInline, never sound. It plays only while in view and
 * while the tab is visible. Under prefers-reduced-motion it is paused and
 * parked on the poster's frame (the clip's own first frame is near-white).
 *
 * The clip's own opening (0-3s of the trimmed file) is the same washed-out
 * segment the poster was chosen to avoid, so playback is seeded to start at
 * POSTER_TIME once metadata loads, instead of autoplaying from 0 — the first
 * motion a visitor sees continues from the poster's frame rather than
 * dipping back into the pale open. It still loops through the full clip
 * (including that segment) afterwards.
 *
 * It also flags the enclosing .hero with `data-paused` whenever it stops, so
 * the hero's one CSS loop (the live dot's pulse, which sits above the video)
 * pauses with it instead of needing an observer of its own.
 */
export function DemoVideo() {
  const ref = useRef<HTMLVideoElement>(null);

  useEffect(() => {
    const video = ref.current;
    if (!video) return;
    const hero = video.closest<HTMLElement>(".hero");
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)");
    let inView = true;
    let seeded = false;

    /* React does not reliably server-render the `muted` attribute, and
       browsers only autoplay muted media. */
    video.muted = true;

    /** Seeks to POSTER_TIME exactly once (metadata permitting), then runs `then`. */
    const seedOnce = (then: () => void) => {
      if (seeded) {
        then();
        return;
      }
      seeded = true;
      const seek = () => {
        try {
          video.currentTime = POSTER_TIME;
        } catch {
          /* not seekable yet; playback will still start from 0 */
        }
        then();
      };
      if (video.readyState >= 1) seek();
      else video.addEventListener("loadedmetadata", seek, { once: true });
    };

    const apply = () => {
      const run = inView && !document.hidden && !reduce.matches;
      hero?.toggleAttribute("data-paused", !run);
      if (reduce.matches) {
        video.pause();
        seedOnce(() => {});
        return;
      }
      if (run) seedOnce(() => video.play().catch(() => {}));
      else video.pause();
    };

    const io = new IntersectionObserver(
      (entries) => {
        inView = entries[0]?.isIntersecting ?? true;
        apply();
      },
      { rootMargin: "120px" },
    );
    io.observe(video);
    apply();
    reduce.addEventListener("change", apply);
    document.addEventListener("visibilitychange", apply);
    return () => {
      io.disconnect();
      reduce.removeEventListener("change", apply);
      document.removeEventListener("visibilitychange", apply);
    };
  }, []);

  return (
    <div className="relative aspect-[16/10] bg-raised">
      <video
        ref={ref}
        poster="/hero-poster.webp"
        loop
        muted
        playsInline
        preload="metadata"
        disablePictureInPicture
        aria-label="Demo video wallpaper looping on a Linux desktop"
        className="absolute inset-0 size-full object-cover object-[50%_30%]"
      >
        <source src="/demo-wallpaper.webm" type='video/webm; codecs="av01.0.05M.08"' />
        <source src="/demo-wallpaper.mp4" type="video/mp4" />
      </video>
    </div>
  );
}
