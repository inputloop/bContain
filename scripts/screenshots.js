// Regenerate the README screenshots in docs/screenshots/.
//
//   node scripts/screenshots.js <path-to-bcontain> [dashboard-url]
//
// The dashboard must already be running (`bcontain ui`). CLI shots run the
// real binary and render its actual output; nothing is mocked.

const { chromium } = require("playwright");
const { execFileSync } = require("child_process");
const path = require("path");

const bin = process.argv[2];
const url = process.argv[3] || "http://127.0.0.1:7878/";
const out = path.join(__dirname, "..", "docs", "screenshots");
if (!bin) {
  console.error("usage: node scripts/screenshots.js <bcontain> [url]");
  process.exit(2);
}

const cli = [
  ["doctor", ["doctor"]],
  ["presets", ["presets"]],
  ["list", ["list"]],
  ["run-container", ["run", process.env.SHOT_IMAGE || "alpine:3.20"]],
];

function esc(s) {
  return s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
}

function terminal(cmd, text) {
  return `<!doctype html><meta charset="utf-8"><style>
    body{margin:0;background:#0e1013;padding:24px;font:14px/1.5 "DejaVu Sans Mono",ui-monospace,monospace}
    .win{background:#161a1f;border:1px solid #262c34;border-radius:10px;overflow:hidden;width:max-content;max-width:1100px}
    .bar{display:flex;gap:7px;padding:10px 12px;background:#1d2228}
    .bar i{width:11px;height:11px;border-radius:50%;background:#3a414a;display:block}
    pre{margin:0;padding:14px 18px 18px;color:#e8ebef;white-space:pre-wrap}
    .p{color:#86cc0a}.ok{color:#49c27a}.bad{color:#ef6b6b}.dim{color:#9aa3ae}
  </style><div class="win"><div class="bar"><i></i><i></i><i></i></div><pre><span class="p">$</span> bcontain ${esc(cmd)}
${esc(text)
  .replace(/✔/g, '<span class="ok">✔</span>')
  .replace(/✘/g, '<span class="bad">✘</span>')
  .replace(/^(\[bcontain\].*)$/gm, '<span class="dim">$1</span>')}</pre></div>`;
}

(async () => {
  const browser = await chromium.launch();

  for (const [name, args] of cli) {
    // Merge stderr: progress lines and doctor's hints matter in the shot.
    const shell = `"${bin}" ${args.map((a) => `'${a}'`).join(" ")} 2>&1`;
    let text;
    try {
      text = execFileSync("sh", ["-c", shell], { encoding: "utf8", timeout: 300000 });
    } catch (e) {
      text = e.stdout || String(e);
    }
    const page = await browser.newPage({ viewport: { width: 1200, height: 400 }, deviceScaleFactor: 2 });
    await page.setContent(terminal(args.join(" "), text.trimEnd()));
    await page.locator(".win").screenshot({ path: path.join(out, `cli-${name}.png`) });
    await page.close();
    console.log(`cli-${name}.png`);
  }

  const shots = [
    ["dashboard-light", { width: 1360, height: 900 }, "light"],
    ["dashboard-dark", { width: 1360, height: 900 }, "dark"],
    ["dashboard-mobile", { width: 390, height: 1500 }, "dark"],
  ];
  for (const [name, viewport, colorScheme] of shots) {
    const page = await browser.newPage({ viewport, colorScheme, deviceScaleFactor: 2 });
    await page.goto(url);
    await page.waitForSelector(".check");
    await page.waitForTimeout(500);
    await page.screenshot({ path: path.join(out, `${name}.png`), fullPage: true });
    await page.close();
    console.log(`${name}.png`);
  }
  await browser.close();
})();
