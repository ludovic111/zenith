/**
 * Agent avatars: a bright, rounded shape with two dark capsule eyes and, if you like, one
 * accessory — recognizable at a glance, even at 14 px, and all of a family. Pure, so the
 * interface draws them and zenith writes them as each agent folder's favicon.svg (which
 * zenith code shows next to the project).
 */

export const AVATAR_SHAPES = ["circle", "blob", "square", "pill", "hexagon", "triangle", "heart", "flower"] as const;
export const AVATAR_ACCESSORIES = ["none", "antenna", "sprout", "star", "bow", "crown", "glasses", "headset"] as const;

export type AvatarShape = (typeof AVATAR_SHAPES)[number];
export type AvatarAccessory = (typeof AVATAR_ACCESSORIES)[number];
export type Avatar = { shape: AvatarShape; color: string; accessory: AvatarAccessory };

const BRIGHT = ["#8B5CF6", "#F97316", "#22C55E", "#0EA5E9", "#EC4899", "#EAB308", "#14B8A6", "#6366F1"];

const hash = (s: string) => [...s].reduce((h, c) => (Math.imul(h, 31) + c.charCodeAt(0)) | 0, 7) >>> 0;

/** An avatar for an id, from what the config says and, for the rest, the id itself. */
export function avatarOf(id: string, given: { shape?: string; color?: string; accessory?: string } = {}): Avatar {
  const h = hash(id);
  const shape = (AVATAR_SHAPES as readonly string[]).includes(given.shape ?? "") ? (given.shape as AvatarShape) : AVATAR_SHAPES[h % AVATAR_SHAPES.length];
  const accessory = (AVATAR_ACCESSORIES as readonly string[]).includes(given.accessory ?? "") ? (given.accessory as AvatarAccessory) : "none";
  const color = /^#[0-9a-f]{6}$/i.test(given.color ?? "") ? given.color! : BRIGHT[(h >> 3) % BRIGHT.length];
  return { shape, color, accessory };
}

/** `hex` mixed with white (t > 0) or black (t < 0). */
function mix(hex: string, t: number) {
  const n = parseInt(hex.slice(1), 16);
  const to = t > 0 ? 255 : 0;
  const k = Math.abs(t);
  const c = [n >> 16, (n >> 8) & 255, n & 255].map((v) => Math.round(v + (to - v) * k));
  return `#${c.map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

const INK = "#1B1B1F";

/** The body, in a 40×40 box; where its top is and where the eyes sit. */
function body(shape: AvatarShape, fill: string): { svg: string; top: number; eyes: number } {
  const round = `fill="${fill}" stroke="${fill}" stroke-linejoin="round"`;
  switch (shape) {
    case "square":
      return { svg: `<rect x="6.5" y="9.5" width="27" height="27" rx="9" fill="${fill}"/>`, top: 9.5, eyes: 22.5 };
    case "pill":
      return { svg: `<rect x="4" y="13" width="32" height="21" rx="10.5" fill="${fill}"/>`, top: 13, eyes: 23 };
    case "hexagon": {
      const pts = Array.from({ length: 6 }, (_, i) => {
        const a = (Math.PI / 3) * i;
        return `${(20 + 13 * Math.cos(a)).toFixed(2)},${(23 + 13 * Math.sin(a)).toFixed(2)}`;
      }).join(" ");
      return { svg: `<polygon points="${pts}" ${round} stroke-width="4"/>`, top: 10, eyes: 22.5 };
    }
    case "triangle":
      return { svg: `<polygon points="20,10 34,34 6,34" ${round} stroke-width="5"/>`, top: 8, eyes: 27 };
    case "heart":
      return {
        svg: `<path d="M20 36.5C9.5 29.5 4.5 23.5 4.5 17.5a7.9 7.9 0 0 1 15.5-2.2 7.9 7.9 0 0 1 15.5 2.2c0 6-5 12-15.5 19z" fill="${fill}"/>`,
        top: 11,
        eyes: 21,
      };
    case "flower": {
      const petals = Array.from({ length: 6 }, (_, i) => {
        const a = (Math.PI / 3) * i - Math.PI / 2;
        return `<circle cx="${(20 + 8.5 * Math.cos(a)).toFixed(2)}" cy="${(23 + 8.5 * Math.sin(a)).toFixed(2)}" r="6.2" fill="${fill}"/>`;
      }).join("");
      return { svg: `${petals}<circle cx="20" cy="23" r="9.5" fill="${fill}"/>`, top: 8.5, eyes: 23 };
    }
    case "blob":
      return { svg: `<path d="M20.5 9c8.4 0 14 6.1 14 13.9 0 7.7-6.3 14.1-14.4 14.1C12 37 5.5 31.7 5.5 23.8 5.5 15.2 12.3 9 20.5 9z" fill="${fill}"/>`, top: 9, eyes: 22.5 };
    default:
      return { svg: `<circle cx="20" cy="23" r="14" fill="${fill}"/>`, top: 9, eyes: 22.5 };
  }
}

/** Accessories are drawn for a body whose top is at y = 9, then moved to the real top. */
function accessory(a: AvatarAccessory, color: string, top: number, eyes: number): string {
  const dy = (top - 9).toFixed(2);
  const at = (svg: string) => `<g transform="translate(0 ${dy})">${svg}</g>`;
  switch (a) {
    case "antenna":
      return at(`<path d="M20 9.5V4" stroke="${mix(color, -0.4)}" stroke-width="1.8" stroke-linecap="round"/><circle cx="20" cy="3.4" r="2.6" fill="${mix(color, 0.45)}" stroke="${mix(color, -0.4)}" stroke-width="1.2"/>`);
    case "sprout":
      return at(`<path d="M20 10V5.5" stroke="#2F9E44" stroke-width="1.6" stroke-linecap="round"/><path d="M20 6.5c-1-3-4-4-6.5-3.5.5 2.8 3.5 4.3 6.5 3.5z" fill="#40C057"/><path d="M20 6c.8-2.8 3.6-4 6-3.4-.6 2.6-3.3 4-6 3.4z" fill="#69DB7C"/>`);
    case "star":
      return at(`<path d="M28 1.6l1.5 3 3.3.5-2.4 2.3.6 3.3-3-1.6-3 1.6.6-3.3-2.4-2.3 3.3-.5z" fill="#FFC53D" stroke="#E8A200" stroke-width=".6" stroke-linejoin="round"/>`);
    case "bow":
      return at(`<path d="M27 9.5l-5-3.2v6.4zM27 9.5l5-3.2v6.4z" fill="#FF5FA2" stroke="#E0337F" stroke-width=".8" stroke-linejoin="round"/><circle cx="27" cy="9.5" r="1.7" fill="#E0337F"/>`);
    case "crown":
      return at(`<path d="M13.5 10.5l-1-7 4.2 3.2L20 2l3.3 4.7 4.2-3.2-1 7z" fill="#FFC53D" stroke="#E8A200" stroke-width=".8" stroke-linejoin="round"/>`);
    case "glasses": {
      const y = eyes.toFixed(2);
      return `<g fill="none" stroke="${INK}" stroke-width="1.4"><circle cx="15" cy="${y}" r="4.3"/><circle cx="25" cy="${y}" r="4.3"/><path d="M19.3 ${y}h1.4"/></g>`;
    }
    case "headset": {
      const band = mix(color, -0.45);
      return `<path d="M7 ${(eyes + 1).toFixed(2)}a13 13 0 0 1 26 0" fill="none" stroke="${band}" stroke-width="2" transform="translate(0 ${(top - 9 - 3).toFixed(2)})"/><rect x="3.5" y="${(eyes - 2).toFixed(2)}" width="5" height="7.5" rx="2.2" fill="${band}"/><rect x="31.5" y="${(eyes - 2).toFixed(2)}" width="5" height="7.5" rx="2.2" fill="${band}"/>`;
    }
    default:
      return "";
  }
}

/**
 * The avatar as an SVG string. `blink` makes its eyes blink now and then (never with
 * reduced motion); `id` keeps its gradient apart from other avatars on the page.
 */
export function avatarSvg(a: Avatar, { size = 32, blink = false, id = "a" }: { size?: number; blink?: boolean; id?: string } = {}): string {
  const g = `zav-${id.replace(/[^\w-]/g, "")}`;
  const { svg, top, eyes } = body(a.shape, `url(#${g})`);
  const delay = (hash(id) % 40) / 10;
  const eye = (x: number) =>
    `<rect x="${x - 1.8}" y="${eyes - 3.4}" width="3.6" height="6.8" rx="1.8" fill="${INK}"/><circle cx="${x - 0.5}" cy="${eyes - 1.9}" r=".85" fill="#fff" opacity=".9"/>`;
  return [
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40" width="${size}" height="${size}" aria-hidden="true">`,
    `<defs><radialGradient id="${g}" cx="35%" cy="30%" r="75%"><stop offset="0" stop-color="${mix(a.color, 0.42)}"/><stop offset=".55" stop-color="${a.color}"/><stop offset="1" stop-color="${mix(a.color, -0.18)}"/></radialGradient></defs>`,
    blink
      ? `<style>@keyframes ${g}b{0%,94%,100%{transform:scaleY(1)}97%{transform:scaleY(.15)}}.${g}e{transform-box:fill-box;transform-origin:center;animation:${g}b 6s ${delay}s infinite}@media (prefers-reduced-motion:reduce){.${g}e{animation:none}}</style>`
      : "",
    a.accessory === "headset" ? accessory(a.accessory, a.color, top, eyes) : "",
    svg,
    `<g class="${g}e">${eye(15)}${eye(25)}</g>`,
    a.accessory !== "headset" ? accessory(a.accessory, a.color, top, eyes) : "",
    `</svg>`,
  ].join("");
}
