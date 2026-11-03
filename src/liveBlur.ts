import { invoke } from "@tauri-apps/api/core";
import { setVisibleInterval } from "./hooks/visibleInterval";

// Live blur for the glass styles: the backend keeps a native window behind the notch or dock, cut
// to the surface's outline, and Windows blurs whatever is under it. This sends that outline
// whenever the surface moves, resizes or fades.

export type LiveBlurKind = "blur" | "acrylic";
export const LIVE_BLUR_OPTIONS = ["off", "blur", "acrylic"] as const;

const SVG_NS = "http://www.w3.org/2000/svg";

let kind: LiveBlurKind | null = null;
let dark = true;
let seq = 0;
let lastSent = "";
let frame = 0;
let poll: number | undefined;
let observer: MutationObserver | undefined;
let watched: HTMLElement | null = null;
let sampler: SVGPathElement | undefined;
let cachedPath = "";
let cachedPoints: number[] = [];

export function liveBlurSetting(): LiveBlurKind | null {
  const value = localStorage.getItem("nectar-theme-live-blur");
  return value === "blur" || value === "acrylic" ? value : null;
}

// The fill layer carries the surface's whole outline, flares included, as its clip path.
const surfaceFill = () => document.querySelector<HTMLElement>(".nectar > .surface-fill, .dock > .surface-fill");

function opacity(el: Element | null) {
  let value = 1;
  for (; el && value > 0; el = el.parentElement) value *= Number(getComputedStyle(el).opacity);
  return value;
}

// Points along a path, in the fill layer's own pixels. Straight runs keep only their ends.
function outline(d: string): number[] {
  if (d === cachedPath) return cachedPoints;
  if (!sampler) {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("aria-hidden", "true");
    svg.style.cssText = "position:absolute;width:0;height:0;overflow:hidden;pointer-events:none";
    sampler = document.createElementNS(SVG_NS, "path");
    svg.appendChild(sampler);
    document.body.appendChild(svg);
  }
  sampler.setAttribute("d", d);
  const length = sampler.getTotalLength();
  const steps = Math.min(600, Math.ceil(length / 2));
  const raw: [number, number][] = [];
  for (let i = 0; i < steps; i++) {
    const p = sampler.getPointAtLength((length * i) / steps);
    raw.push([p.x, p.y]);
  }
  const points: number[] = [];
  raw.forEach(([x, y], i) => {
    const [px, py] = raw[(i + raw.length - 1) % raw.length];
    const [nx, ny] = raw[(i + 1) % raw.length];
    if (Math.abs((x - px) * (ny - y) - (y - py) * (nx - x)) > 0.01) points.push(x, y);
  });
  cachedPath = d;
  cachedPoints = points;
  return points;
}

// Watches the fill layer and everything above it, since the surface animates its own size and
// position, and the window can slide or fade a parent.
function watch(fill: HTMLElement | null) {
  if (fill === watched || !observer) return;
  observer.disconnect();
  watched = fill;
  for (let el = fill; el && el !== document.documentElement; el = el.parentElement) {
    observer.observe(el, { attributes: true, attributeFilter: ["style", "class"] });
  }
}

function measure() {
  frame = 0;
  if (!kind) return;
  const fill = surfaceFill();
  watch(fill);
  let points: number[] = [];
  const clip = fill?.style.clipPath.match(/path\(["'](.+)["']\)/);
  // A surface that's mostly faded out shouldn't leave its blur behind.
  if (fill && clip && fill.offsetWidth > 0 && fill.offsetHeight > 0 && opacity(fill) >= 0.5) {
    const r = fill.getBoundingClientRect();
    // From the layer's own pixels to the window's, through the UI scale and any transform
    const sx = r.width / fill.offsetWidth;
    const sy = r.height / fill.offsetHeight;
    points = outline(clip[1]).map((v, i) => Math.round((i % 2 === 0 ? r.left + v * sx : r.top + v * sy) * 2) / 2);
  }
  const key = `${kind} ${dark} ${points.join(",")}`;
  if (key === lastSent) return;
  lastSent = key;
  invoke("set_live_blur", { kind, dark, seq: ++seq, points }).catch(() => {
    lastSent = "";
  });
}

function schedule() {
  if (!frame) frame = requestAnimationFrame(measure);
}

let supported: Promise<boolean> | undefined;

// False before Windows 10 1809, where the glass falls back to the wallpaper.
export function liveBlurSupported() {
  supported ??= invoke<boolean>("live_blur_supported").catch(() => false);
  return supported;
}

export function startLiveBlur(next: LiveBlurKind, isDark: boolean) {
  kind = next;
  dark = isDark;
  if (!observer) {
    observer = new MutationObserver(schedule);
    window.addEventListener("resize", schedule);
    poll = setVisibleInterval(schedule, 1000);
  }
  watched = null;
  schedule();
}

export function stopLiveBlur() {
  if (!observer) return;
  observer.disconnect();
  observer = undefined;
  watched = null;
  window.removeEventListener("resize", schedule);
  window.clearInterval(poll);
  poll = undefined;
  if (frame) cancelAnimationFrame(frame);
  frame = 0;
  kind = null;
  lastSent = "";
  invoke("set_live_blur", { kind: null, dark, seq: ++seq, points: [] }).catch(() => {});
}
