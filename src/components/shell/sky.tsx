const BLOBS = [
  { c: "#00D2FF", top: "-12%", left: "-8%", x: "10vw", y: "8vh", d: "42s" },
  { c: "#FF8A4C", top: "55%", left: "70%", x: "-12vw", y: "-10vh", d: "36s" },
  { c: "#B18CFF", top: "-18%", left: "55%", x: "-8vw", y: "12vh", d: "48s" },
  { c: "#B6F23A", top: "70%", left: "-10%", x: "14vw", y: "-6vh", d: "52s" },
  { c: "#FF6FB5", top: "25%", left: "30%", x: "6vw", y: "10vh", d: "40s" },
];

/** Background: five glows drifting slowly under a starry sky. */
export function Sky() {
  return (
    <>
      <div className="sky" aria-hidden>
        <div className="stars" />
        {BLOBS.map((b) => (
          <div
            key={b.c}
            className="blob"
            style={{ background: `radial-gradient(circle, ${b.c} 0%, transparent 65%)`, top: b.top, left: b.left, ["--x" as string]: b.x, ["--y" as string]: b.y, ["--d" as string]: b.d }}
          />
        ))}
        <div className="absolute inset-0 bg-[radial-gradient(ellipse_at_top,transparent_0%,#07060d_75%)]" />
      </div>
      <div className="grain" aria-hidden />
    </>
  );
}
