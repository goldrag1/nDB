// ── All-papers GPU point-cloud backdrop ───────────────────────────────────
// The 3d-force-graph interactive layer maxes out at a few thousand meshes (one
// mesh + force-sim body per node). To show ALL ~6.17M papers we render them as
// a single THREE.Points cloud (one GPU draw call) added to the same scene, as a
// non-interactive backdrop. The force-graph stays the focused interactive layer
// on top (labels, hover, click, citation links).
//
// Data: GET api/view/cloud -> cloud.bin (binary, see server build_cloud_file):
//   header [magic "NDCLOUD1"(8) | u32 count | u32 0], then count x record
//   [f32 x | f32 y | f32 z | u16 field_idx | u16 size_q] (16 B/record).
// field_idx indexes the clusters array -> colored by the SAME field hue as the
// nodes. Positions are the EXACT server layout, so points coincide with nodes.
(function () {
  "use strict";

  // Match the explorer's fieldColor(): hsl(hash(field) % 360, 68%, 62%).
  function fieldHue(name) {
    let h = 0;
    for (const c of name) h = (Math.imul(h, 31) + c.charCodeAt(0)) >>> 0;
    return h % 360;
  }
  function hslToRgb(h, s, l) {
    h /= 360;
    const a = s * Math.min(l, 1 - l);
    const f = (n) => {
      const k = (n + h * 12) % 12;
      return l - a * Math.max(-1, Math.min(k - 3, Math.min(9 - k, 1)));
    };
    return [f(0), f(8), f(4)];
  }
  const OTHER_RGB = hslToRgb(fieldHue("Other"), 0.68, 0.62);

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src; s.onload = resolve; s.onerror = () => reject(new Error("load " + src));
      document.head.appendChild(s);
    });
  }

  async function ensureThree() {
    if (window.THREE) return window.THREE;
    // 3d-force-graph bundles THREE privately; load our own compatible copy so
    // we can construct geometry. The WebGLRenderer draws any isObject3D added
    // to the scene, so a separate THREE copy renders fine.
    await loadScript("https://unpkg.com/three@0.150.1/build/three.min.js");
    return window.THREE;
  }

  function waitForGraph() {
    return new Promise((resolve) => {
      const t = setInterval(() => {
        if (window.Graph && typeof window.Graph.scene === "function") {
          clearInterval(t); resolve(window.Graph);
        }
      }, 150);
    });
  }

  async function fetchClusterColors() {
    // field_idx -> [r,g,b], same order the server emits (clusters array).
    try {
      const c = await (await fetch("api/view/clusters", { cache: "no-store" })).json();
      return (c.nodes || []).map((n) =>
        n.field === "Other" ? OTHER_RGB : hslToRgb(fieldHue(n.field), 0.68, 0.62)
      );
    } catch (e) { return []; }
  }

  async function buildCloud() {
    const Graph = await waitForGraph();
    let probe;
    try { probe = await fetch("api/health", { cache: "no-store" }); } catch (e) { return; }
    if (!probe || !probe.ok) return; // LIVE mode only (static graph.json has no cloud)

    const THREE = await ensureThree();
    if (!THREE) { console.warn("cloud-layer: THREE unavailable"); return; }

    const colorLut = await fetchClusterColors();

    const resp = await fetch("api/view/cloud", { cache: "no-store" });
    if (!resp.ok) { console.warn("cloud-layer: /view/cloud", resp.status); return; }
    const buf = await resp.arrayBuffer();
    const dv = new DataView(buf);

    const magic = String.fromCharCode.apply(null, new Uint8Array(buf, 0, 8));
    if (magic !== "NDCLOUD1") { console.warn("cloud-layer: bad magic", magic); return; }
    const count = dv.getUint32(8, true);
    const REC = 16;

    const positions = new Float32Array(count * 3);
    const colors = new Float32Array(count * 3);
    let off = 16;
    for (let i = 0; i < count; i++, off += REC) {
      positions[i * 3]     = dv.getFloat32(off, true);
      positions[i * 3 + 1] = dv.getFloat32(off + 4, true);
      positions[i * 3 + 2] = dv.getFloat32(off + 8, true);
      const fi = dv.getUint16(off + 12, true);
      const rgb = colorLut[fi] || OTHER_RGB;
      colors[i * 3] = rgb[0]; colors[i * 3 + 1] = rgb[1]; colors[i * 3 + 2] = rgb[2];
    }

    const geom = new THREE.BufferGeometry();
    geom.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    geom.setAttribute("color", new THREE.BufferAttribute(colors, 3));
    const mat = new THREE.PointsMaterial({
      size: 2.0,
      sizeAttenuation: true,
      vertexColors: true,
      transparent: true,
      opacity: 0.5,
      depthWrite: false,
    });
    const points = new THREE.Points(geom, mat);
    points.name = "ndb-paper-cloud";
    points.renderOrder = -1; // behind the interactive nodes
    Graph.scene().add(points);

    window.__cloud = { points, count, setOpacity: (o) => { mat.opacity = o; }, setSize: (s) => { mat.size = s; } };
    console.log("cloud-layer: rendered " + count.toLocaleString() + " papers as a GPU point cloud");

    // Camera-distance LOD: fade the cloud in on zoom-OUT (whole universe), out
    // on zoom-IN (interactive nodes dominate).
    const controls = Graph.controls && Graph.controls();
    if (controls && controls.addEventListener) {
      let base = 0;
      const update = () => {
        const cp = Graph.cameraPosition();
        const d = Math.hypot(cp.x || 0, cp.y || 0, cp.z || 0);
        if (!base) base = d || 1;
        mat.opacity = Math.max(0.1, Math.min(0.55, (d / base) * 0.55));
      };
      controls.addEventListener("change", () => { clearTimeout(window.__cloudT); window.__cloudT = setTimeout(update, 80); });
    }
  }

  if (document.readyState === "complete") setTimeout(buildCloud, 1500);
  else window.addEventListener("load", () => setTimeout(buildCloud, 1500));
})();
