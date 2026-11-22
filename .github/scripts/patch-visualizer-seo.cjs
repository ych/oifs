// .github/scripts/patch-visualizer-seo.cjs
// Idempotently enriches docs/openwiki-visualizer/index.html with SEO metadata and
// canonical links back to the OIFS homepage and GitHub repository after `openwiki visualize`.

const fs = require("fs");
const path = require("path");

const targetPath = path.resolve(
  process.argv[2] || "docs/openwiki-visualizer/index.html"
);

let html = fs.readFileSync(targetPath, "utf8");

// 1. Update page title
html = html.replace(
  "<title>OpenWiki visualizer</title>",
  "<title>OIFS Architecture Wiki — OpenWiki Visualizer</title>"
);

// 2. Inject SEO meta description + canonical link if missing
if (!html.includes('rel="canonical"')) {
  const seoHead = [
    '<meta name="description" content="Interactive architecture graph and technical wiki for OIFS (O\'s Inode File System) — a high-performance single-file POSIX filesystem in Rust." />',
    '<link rel="canonical" href="https://ych.github.io/oifs/openwiki-visualizer/" />',
  ].join("\n");
  html = html.replace(
    "</title>",
    `</title>\n${seoHead}`
  );
}

// 3. Inject subtle Home + GitHub links into topbar if missing
if (!html.includes("https://github.com/ych/oifs")) {
  const navLinks = [
    '<a href="../" style="color:inherit;text-decoration:none;font-size:13px;font-weight:500;opacity:0.85;margin-right:12px;">OIFS Home</a>',
    '<a href="https://github.com/ych/oifs" rel="noopener" style="color:inherit;text-decoration:none;font-size:13px;font-weight:600;opacity:0.9;margin-right:14px;">GitHub ↗</a>',
  ].join("\n  ");
  html = html.replace(
    '<div class="live-pill',
    `${navLinks}\n  <div class="live-pill`
  );
}

fs.writeFileSync(targetPath, html, "utf8");
console.log(`[VisualizerSEO] Patched ${targetPath}`);
